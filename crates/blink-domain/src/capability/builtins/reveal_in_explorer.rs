//! `reveal_in_explorer` Capability（0.14.2 §2.3；0.27.3 多路径扩展）。
//!
//! 从 Action 提升为 Capability——AI 常用（在资源管理器中定位搜索结果文件）。
//! 入参：`{ "path": "..." }`（单路径，兼容保留）或 `{ "paths": [...] }`
//! （0.27.3 文件选中感知：多选按父目录分组定位）。
//!
//! **多路径策略**（§5.3.4）：按父目录分组，每组一次 `explorer /select`；
//! 默认最多 5 个目录（限制窗口扇出），超过要求用户缩小选择。

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Value, json};

use crate::capability::{
    AiDefault, Capability, CapabilityError, CapabilityPolicy, CapabilityResult, CapabilitySchema,
    ConfirmationPolicy, DangerClass, InvokeContext, McpDefault, OriginSet, RuntimeRequirement,
};

/// 多路径定位的目录分组上限（§5.3.4：限制窗口扇出）。
const MAX_REVEAL_GROUPS: usize = 5;

/// `reveal_in_explorer` — 在 Windows 资源管理器中定位文件（高亮显示）。
///
/// 入参：`{ "path": "C:\\..." }` 或 `{ "paths": ["C:\\...", ...] }`（二选一）
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
            description: "Reveal files in Windows Explorer (highlight them in their folder). Accepts a single `path` or multiple `paths` (grouped by folder, max 5 folders)".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute file path to reveal (single-path form)"
                    },
                    "paths": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Absolute file paths to reveal (multi-path form, grouped by parent folder, max 5 folders)"
                    }
                }
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
        let paths = parse_reveal_paths(&args)?;
        let groups = group_paths_by_parent(&paths)?;

        for (parent, group_paths) in &groups {
            tracing::debug!(dir = %parent, count = group_paths.len(), "reveal_in_explorer capability: 在资源管理器中显示");
            #[cfg(target_os = "windows")]
            {
                // explorer /select,p1,p2,...：同组（同目录）路径一次定位多选。
                // 逗号分隔语法无法转义路径中的逗号——组内任一路径含逗号时
                // 退化为逐路径 spawn（选中态会被后者覆盖，可接受的降级）。
                let batches: Vec<String> = if group_paths.iter().any(|p| p.contains(',')) {
                    group_paths.clone()
                } else {
                    vec![group_paths.join(",")]
                };
                for batch in batches {
                    let select_arg = format!("/select,{batch}");
                    let status =
                        blink_infra::platform::no_window(std::process::Command::new("explorer.exe"))
                            .arg(select_arg)
                            .spawn();
                    if let Err(e) = status {
                        tracing::error!(error = %e, dir = %parent, "调用 explorer.exe 失败");
                        return Err(CapabilityError::Internal {
                            detail: format!("调用 explorer.exe 失败: {e}"),
                        });
                    }
                }
            }
        }

        let summary = match paths.len() {
            1 => format!("已在资源管理器中定位: {}", paths[0]),
            n => format!("已在资源管理器中定位 {n} 项（{} 个目录）", groups.len()),
        };
        Ok(CapabilityResult::Done { summary })
    }
}

/// 参数解析：`path`（单路径兼容）或 `paths`（多路径）二选一（0.27.3 抽出为纯函数）。
/// 数组中混入非字符串项显式报错（与「非数组/空数组」一致，不静默少 reveal）。
fn parse_reveal_paths(args: &Value) -> Result<Vec<String>, CapabilityError> {
    if let Some(list) = args.get("paths") {
        let raw = list
            .as_array()
            .ok_or_else(|| CapabilityError::InvalidArgs {
                detail: "reveal_in_explorer: paths 必须是字符串数组".into(),
            })?;
        let mut parsed = Vec::with_capacity(raw.len());
        for (index, value) in raw.iter().enumerate() {
            let Some(s) = value.as_str() else {
                return Err(CapabilityError::InvalidArgs {
                    detail: format!(
                        "reveal_in_explorer: paths 第 {} 项不是字符串",
                        index + 1
                    ),
                });
            };
            let s = s.trim();
            if !s.is_empty() {
                parsed.push(s.to_string());
            }
        }
        if parsed.is_empty() {
            return Err(CapabilityError::InvalidArgs {
                detail: "reveal_in_explorer: paths 不能为空".into(),
            });
        }
        return Ok(parsed);
    }
    let path = args
        .get("path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| CapabilityError::InvalidArgs {
            detail: "reveal_in_explorer: 缺少 path 参数".into(),
        })?;
    Ok(vec![path.to_string()])
}

/// 多路径按父目录分组（BTreeMap：目录顺序稳定），超 [`MAX_REVEAL_GROUPS`] 组拒绝。
fn group_paths_by_parent(
    paths: &[String],
) -> Result<BTreeMap<String, Vec<String>>, CapabilityError> {
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for path in paths {
        let parent = std::path::Path::new(path)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        groups.entry(parent).or_default().push(path.clone());
    }
    if groups.len() > MAX_REVEAL_GROUPS {
        return Err(CapabilityError::InvalidArgs {
            detail: format!(
                "reveal_in_explorer: 选中项跨 {count} 个目录，超过 {MAX_REVEAL_GROUPS} 个上限，请缩小选择范围",
                count = groups.len()
            ),
        });
    }
    Ok(groups)
}

inventory::submit!(crate::capability::CapabilityEntry {
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
    fn schema_has_path_parameters() {
        let s = RevealInExplorer.schema();
        assert_eq!(s.name, "reveal_in_explorer");
        assert_eq!(s.parameters["properties"]["path"]["type"], "string");
        assert_eq!(s.parameters["properties"]["paths"]["type"], "array");
    }

    #[test]
    fn schema_description_non_empty() {
        let s = RevealInExplorer.schema();
        assert!(!s.description.is_empty());
    }

    // ── 参数解析与分组纯逻辑（explorer spawn 属 GUI 层，手动验证）──

    #[test]
    fn parse_missing_both_forms_rejected() {
        assert!(matches!(
            parse_reveal_paths(&json!({})),
            Err(CapabilityError::InvalidArgs { .. })
        ));
    }

    #[test]
    fn parse_invalid_or_empty_paths_rejected() {
        for args in [json!({"paths": "not-array"}), json!({"paths": []})] {
            let err = parse_reveal_paths(&args).unwrap_err();
            assert!(
                matches!(&err, CapabilityError::InvalidArgs { detail } if detail.contains("paths")),
                "args={args} 应拒绝，实际 {err:?}"
            );
        }
    }

    #[test]
    fn parse_single_path_compat() {
        let paths = parse_reveal_paths(&json!({"path": "C:\\tmp\\a.txt"})).unwrap();
        assert_eq!(paths, vec!["C:\\tmp\\a.txt"]);
        // 空白字符串视为缺失
        assert!(parse_reveal_paths(&json!({"path": "  "})).is_err());
    }

    /// 跨目录超上限 → InvalidArgs 且文案提示缩小范围（spawn 前即被拒）。
    #[test]
    fn too_many_folders_rejected() {
        let paths: Vec<String> = (0..6)
            .map(|i| format!("C:\\dir{i}\\file{i}.txt"))
            .collect();
        let err = group_paths_by_parent(&paths).unwrap_err();
        match err {
            CapabilityError::InvalidArgs { detail } => {
                assert!(detail.contains("6 个目录"), "文案含实际目录数: {detail}");
                assert!(detail.contains("缩小选择"), "文案指引缩小范围: {detail}");
            }
            other => panic!("应 InvalidArgs，实际 {other:?}"),
        }
    }

    /// 同目录多文件归一组；单条路径单组——分组结果稳定有序。
    #[test]
    fn group_by_parent_orders_and_merges() {
        let paths = vec![
            "C:\\b\\2.txt".to_string(),
            "C:\\a\\1.txt".to_string(),
            "C:\\a\\3.txt".to_string(),
        ];
        let groups = group_paths_by_parent(&paths).unwrap();
        let dirs: Vec<&String> = groups.keys().collect();
        assert_eq!(dirs, vec!["C:\\a", "C:\\b"], "BTreeMap 保证目录字典序");
        assert_eq!(groups["C:\\a"].len(), 2);
    }
}
