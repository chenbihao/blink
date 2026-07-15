//! Tool 分组（0.9.3）—— 多 Action 投影为一个 AI tool。
//!
//! **动机**：12 个内置动作 = 12 个 tool，prompt token 膨胀。
//! 把语义相近的 Action 聚合为一个 tool，AI 返回 `tool_call(name="system_action", arguments={action:"lock"})`，
//! 路由层解析 `action` 参数分派到具体 Action。
//!
//! **设计原则**：
//! - 聚合只在 AI 投影层，不影响手动执行路径（ActionRegistry.get(id) → execute）
//! - 插件 tool 不参与聚合，保持独立
//! - 分组静态定义，运行时按 ActionRegistry 白名单过滤
//!
//! **0.9.7 Step 4 扩展**：`build_aggregated_tools` 同时收集 CapabilityRegistry 的能力 schema，
//! 让 AI tool_call 能命中 Capability（search_files / capture_screen 等）。
//! Capability 不参与分组聚合——保持独立 tool，与插件 tool 同模式。

use super::registry::ActionRegistry;
use super::schema::ActionSchema;
use crate::domain::capability::CapabilityRegistry;

/// Tool 分组定义。
///
/// 静态数据，描述"哪些 Action 聚合为一个 AI tool"。
pub struct ToolGroup {
    /// AI tool 名称（如 "system_action"）
    pub name: &'static str,
    /// AI tool 描述（供 system prompt + tool schema 使用）
    pub description: &'static str,
    /// 可选的动作 id 白名单（按此列表顺序生成 action enum）
    pub action_ids: &'static [&'static str],
}

/// 系统操作类（全部 Dangerous）。
pub const SYSTEM_GROUP: ToolGroup = ToolGroup {
    name: "system_action",
    description: "系统操作：锁屏(lock)、关机(shutdown)、重启(restart)、睡眠(sleep)、清除历史(clear_history)",
    action_ids: &["lock", "shutdown", "restart", "sleep", "clear_history"],
};

/// Blink 本体类。
pub const BLINK_GROUP: ToolGroup = ToolGroup {
    name: "blink_action",
    description: "Blink 功能：打开设置(open_settings)、打开日志(open_logs)、打开数据目录(open_data_dir)、退出(exit_blink)",
    action_ids: &["open_settings", "open_logs", "open_data_dir", "exit_blink"],
};

/// 文件/URL 操作类（全部参数化，Safe）。
pub const FILE_GROUP: ToolGroup = ToolGroup {
    name: "file_action",
    description: "打开目标：URL(open_url)、文件/目录(open_path)、在资源管理器显示(reveal_in_explorer)",
    action_ids: &["open_url", "open_path", "reveal_in_explorer"],
};

/// 所有内置分组（顺序影响 system prompt 中的工具列表顺序）。
pub const BUILTIN_GROUPS: &[&ToolGroup] = &[&SYSTEM_GROUP, &BLINK_GROUP, &FILE_GROUP];

impl ToolGroup {
    /// 从分组构造聚合 ActionSchema。
    ///
    /// parameters 包含：
    /// - `action` 字段（枚举，列出 group 内所有 action id）
    /// - group 内各 Action schema 的 parameters 合并（取并集,同名参数 first-wins 并 warn）
    pub fn to_schema(&self, registry: &ActionRegistry) -> ActionSchema {
        // 收集 group 内所有 action 的 schema
        let action_schemas: Vec<ActionSchema> = self
            .action_ids
            .iter()
            .filter_map(|id| registry.get(id).map(|a| a.schema()))
            .collect();

        // 枚举所有 action id（只包含 registry 中实际存在的）
        let action_enum: Vec<&str> = action_schemas.iter().map(|s| s.name.as_str()).collect();

        // 合并参数：取各 action 参数的并集
        let mut all_properties = serde_json::Map::new();
        let mut all_required = vec!["action".to_string()];

        for schema in &action_schemas {
            // 提取 properties（同名参数 first-wins，后续跳过并 warn）
            if let Some(props) = schema.parameters.get("properties") {
                if let Some(obj) = props.as_object() {
                    for (k, v) in obj {
                        if k == "action" {
                            continue;
                        }
                        if all_properties.contains_key(k) {
                            tracing::warn!(
                                group = self.name,
                                param = %k,
                                skipped_from = %schema.name,
                                "ToolGroup 参数合并:同名参数跳过(first-wins)"
                            );
                        } else {
                            all_properties.insert(k.clone(), v.clone());
                        }
                    }
                }
            }
            // 提取 required（去重）
            if let Some(req) = schema.parameters.get("required") {
                if let Some(arr) = req.as_array() {
                    for r in arr {
                        if let Some(s) = r.as_str() {
                            if s != "action" && !all_required.contains(&s.to_string()) {
                                all_required.push(s.to_string());
                            }
                        }
                    }
                }
            }
        }

        // 构造聚合 schema
        // 先构造 properties 中的 action 字段
        let action_prop = serde_json::json!({
            "type": "string",
            "enum": action_enum,
            "description": "要执行的操作"
        });

        // 合并 action 字段和其他参数
        let mut properties = serde_json::Map::new();
        properties.insert("action".to_string(), action_prop);
        for (k, v) in all_properties {
            properties.insert(k, v);
        }

        let parameters = serde_json::json!({
            "type": "object",
            "properties": properties,
            "required": all_required
        });

        ActionSchema {
            name: self.name.to_string(),
            description: self.description.to_string(),
            parameters,
        }
    }

    /// 检查给定 action_id 是否属于此分组。
    pub fn contains(&self, action_id: &str) -> bool {
        self.action_ids.contains(&action_id)
    }
}

/// 检查给定 action_id 是否属于某个内置分组。
pub fn is_grouped_action(action_id: &str) -> bool {
    BUILTIN_GROUPS.iter().any(|g| g.contains(action_id))
}

/// 根据 tool_call name 查找对应的分组。
pub fn find_group(name: &str) -> Option<&'static ToolGroup> {
    BUILTIN_GROUPS.iter().find(|g| g.name == name).copied()
}

/// 构建聚合后的 tools 列表（供 AI 路由使用）。
///
/// 三源归一（0.9.7 Step 4）：
/// 1. 内置动作 → 按分组聚合为 3 个 tool
/// 2. 插件 tool → 保持独立（跳过已分组的内置动作）
/// 3. Capability → 独立 tool（不参与分组，schema 直接投影为 ActionSchema）
///
/// **Capability 与 Action 的 name 冲突策略**：Capability 优先——
/// AI tool_call 命中时 `handle_ai_tool_calls` 先查 CapabilityRegistry。
/// 理论上不应冲突（Action id 如 "lock" vs Capability id 如 "search_files"）。
pub fn build_aggregated_tools(
    registry: &ActionRegistry,
    cap_registry: &CapabilityRegistry,
) -> Vec<ActionSchema> {
    let mut tools = Vec::new();

    // 1. 内置动作 → 按分组聚合
    for group in BUILTIN_GROUPS {
        let schema = group.to_schema(registry);
        // 只有当分组内有实际动作时才添加
        if !schema
            .parameters
            .get("properties")
            .and_then(|p| p.get("action"))
            .and_then(|a| a.get("enum"))
            .and_then(|e| e.as_array())
            .map_or(true, |a| a.is_empty())
        {
            tools.push(schema);
        }
    }

    // 2. 插件 tool → 保持独立（跳过已分组的内置动作）
    for id in registry.ids() {
        if is_grouped_action(&id) {
            continue;
        }
        if let Some(action) = registry.get(&id) {
            tools.push(action.schema());
        }
    }

    // 3. Capability → 独立 tool（不参与分组，schema 直接投影为 ActionSchema）
    // CapabilitySchema 与 ActionSchema 结构相同（name/description/parameters），
    // 直接字段拷贝投影——零适配，0.11 MCP 派生也从这份 schema 出。
    for cap_schema in cap_registry.list() {
        // 冲突检测：若 Action 已有同名 tool，warn 并跳过（Capability 优先在 resolve 阶段体现，
        // schema 层不重复发给 LLM——避免模型困惑于两个同名 tool）
        let name = &cap_schema.name;
        if tools.iter().any(|t| &t.name == name) {
            tracing::warn!(
                tool = %name,
                "build_aggregated_tools: Capability name 与已有 Action tool 冲突,跳过 Capability schema"
            );
            continue;
        }
        tools.push(ActionSchema {
            name: cap_schema.name,
            description: cap_schema.description,
            parameters: cap_schema.parameters,
        });
    }

    tools
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_group_contains_expected_actions() {
        assert!(SYSTEM_GROUP.contains("lock"));
        assert!(SYSTEM_GROUP.contains("shutdown"));
        assert!(SYSTEM_GROUP.contains("restart"));
        assert!(SYSTEM_GROUP.contains("sleep"));
        assert!(SYSTEM_GROUP.contains("clear_history"));
        assert!(!SYSTEM_GROUP.contains("open_settings"));
    }

    #[test]
    fn blink_group_contains_expected_actions() {
        assert!(BLINK_GROUP.contains("open_settings"));
        assert!(BLINK_GROUP.contains("open_logs"));
        assert!(BLINK_GROUP.contains("open_data_dir"));
        assert!(BLINK_GROUP.contains("exit_blink"));
        assert!(!BLINK_GROUP.contains("lock"));
    }

    #[test]
    fn file_group_contains_expected_actions() {
        assert!(FILE_GROUP.contains("open_url"));
        assert!(FILE_GROUP.contains("open_path"));
        assert!(FILE_GROUP.contains("reveal_in_explorer"));
        assert!(!FILE_GROUP.contains("open_settings"));
    }

    #[test]
    fn is_grouped_action_returns_true_for_builtin() {
        assert!(is_grouped_action("lock"));
        assert!(is_grouped_action("open_settings"));
        assert!(is_grouped_action("open_url"));
    }

    #[test]
    fn is_grouped_action_returns_false_for_unknown() {
        assert!(!is_grouped_action("unknown_action"));
        assert!(!is_grouped_action("plugin:some_tool"));
    }

    #[test]
    fn find_group_returns_correct_group() {
        let g = find_group("system_action").unwrap();
        assert_eq!(g.name, "system_action");

        let g = find_group("blink_action").unwrap();
        assert_eq!(g.name, "blink_action");

        let g = find_group("file_action").unwrap();
        assert_eq!(g.name, "file_action");

        assert!(find_group("unknown").is_none());
    }
}
