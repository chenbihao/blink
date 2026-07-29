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

// 0.14.4: FILE_GROUP 已删除——open_url / open_path / reveal_in_explorer
// 的 Action 版本已删除，AI tool 池只走 Capability 版本（独立 tool，不参与分组聚合）。

/// 所有内置分组（顺序影响 system prompt 中的工具列表顺序）。
pub const BUILTIN_GROUPS: &[&ToolGroup] = &[&SYSTEM_GROUP, &BLINK_GROUP];

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
/// 两源归一（0.13.7 起插件迁入 Capability，三源→两源）：
/// 1. 内置 Action → 按分组聚合为 3 个 tool（system/blink/file）
/// 2. Capability → 独立 tool（含 builtin 能力 + 插件 tool，schema 直接投影为 ActionSchema）
///
/// **历史**：0.9.3-0.13.6 插件走 ActionRegistry（独立 tool），0.13.7 迁入 CapabilityRegistry，
/// 消除 ActionOutcome::Items / CapabilityResult::Items 双体系重叠。
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

    // 2. 未分组的 Action → 独立 tool（防御性：未来若有未分组 builtin 不漏）
    for id in registry.ids() {
        if is_grouped_action(&id) {
            continue;
        }
        if let Some(action) = registry.get(&id) {
            tools.push(action.schema());
        }
    }

    // 3. Capability → 独立 tool（含 builtin 能力 + 插件 tool，不参与分组）
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

/// 参数 schema 动态注入插件 settings（0.11.1 §2.3b）。
///
/// **投影层改动**：不改 manifest 的 `parameters` 原文，生成新的 `ActionSchema`。
/// 调用方（`service.rs` AI lane）每次 AI 请求时从 `PluginEngine` 取 settings +
/// 从 manifest `ToolDef.setting_bindings` 取绑定映射，调此函数注入。
///
/// **注入规则**（文档 §2.3b）：对 `bindings` 中每个 `(param_name, setting_key)`：
/// 1. 从 settings 读 `setting_key` 的值；跳过 null / 空字符串
/// 2. **required → optional**：从 `parameters.required` 移除 `param_name`
/// 3. **注入 default**：`parameters.properties[param_name].default = <setting_value>`
/// 4. **description 增强**：追加"（默认: {value}）"
///
/// **为什么放 group.rs 而非 plugin 域**：此函数是纯投影逻辑（输入 ActionSchema +
/// settings JSON + bindings map，输出 ActionSchema），不依赖 `PluginEngine` /
/// `PluginHandle` 等 plugin 域类型。放 execution 域避免 execution → plugin 循环依赖。
/// 调用方（service.rs）已 import 两个域，负责组装参数。
pub fn inject_plugin_settings(
    mut schema: ActionSchema,
    settings: Option<&serde_json::Value>,
    bindings: &std::collections::HashMap<String, String>,
) -> ActionSchema {
    let Some(settings) = settings else {
        return schema; // 无 settings 配置，原样返回
    };
    if bindings.is_empty() {
        return schema; // 无绑定声明，原样返回
    }
    let Some(params) = schema.parameters.as_object_mut() else {
        return schema; // parameters 非 object（畸形 schema），不处理
    };

    for (param_name, setting_key) in bindings {
        let Some(setting_val) = settings.get(setting_key) else {
            continue; // 该 setting 未配置，跳过
        };
        // 跳过 null 和空字符串——视为"未配置"
        let is_empty = setting_val.is_null() || setting_val.as_str().is_some_and(|s| s.is_empty());
        if is_empty {
            continue;
        }

        // 1. required → optional：从 required 数组移除该参数
        if let Some(req_arr) = params.get_mut("required").and_then(|r| r.as_array_mut()) {
            req_arr.retain(|r| r.as_str() != Some(param_name));
        }

        // 2. 注入 default + 3. description 增强
        if let Some(props) = params.get_mut("properties").and_then(|p| p.as_object_mut()) {
            if let Some(param) = props.get_mut(param_name).and_then(|p| p.as_object_mut()) {
                param.insert("default".to_string(), setting_val.clone());
                if let Some(desc) = param.get("description").and_then(|d| d.as_str()) {
                    // 字符串 setting 取原始值（避免 Value::Display 带引号），
                    // 非 string（bool/number）用 Value::Display
                    let setting_display = setting_val
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| setting_val.to_string());
                    let enhanced = format!("{desc}（默认: {setting_display}）");
                    param.insert(
                        "description".to_string(),
                        serde_json::Value::String(enhanced),
                    );
                }
            }
        }
    }

    // 清理：若 required 变空数组，移除整个 required 字段（更干净的 schema）
    if let Some(req_arr) = params.get("required").and_then(|r| r.as_array()) {
        if req_arr.is_empty() {
            params.remove("required");
        }
    }

    schema
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

// 0.14.4: FILE_GROUP 已删除，不再测试
// #[test]
// fn file_group_contains_expected_actions() { ... }

    #[test]
    fn is_grouped_action_returns_true_for_builtin() {
        assert!(is_grouped_action("lock"));
        assert!(is_grouped_action("open_settings"));
        // 0.14.4: open_url 不再属于任何分组（FILE_GROUP 已删除）
        assert!(!is_grouped_action("open_url"));
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

        // 0.14.4: FILE_GROUP 已删除
        assert!(find_group("file_action").is_none());

        assert!(find_group("unknown").is_none());
    }

    // ── 0.11.1 §2.3b：inject_plugin_settings 参数动态注入 ──────────────────

    use std::collections::HashMap;

    fn weather_schema_with_city_required() -> ActionSchema {
        // 模拟 weather 插件的原始 schema（city 是 required）
        ActionSchema {
            name: "builtin.weather:get_weather".into(),
            description: "查询天气".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "city": {
                        "type": "string",
                        "description": "城市名称"
                    },
                    "unit": {
                        "type": "string",
                        "description": "温度单位",
                        "enum": ["celsius", "fahrenheit"],
                        "default": "celsius"
                    }
                },
                "required": ["city"]
            }),
        }
    }

    #[test]
    fn inject_returns_schema_unchanged_when_no_settings() {
        // settings = None → 原样返回（老插件无配置）
        let schema = weather_schema_with_city_required();
        let mut bindings = HashMap::new();
        bindings.insert("city".to_string(), "default_city".to_string());

        let result = inject_plugin_settings(schema, None, &bindings);
        // required 仍是 ["city"]
        let required = result.parameters["required"].as_array().unwrap();
        assert!(required.contains(&serde_json::json!("city")));
    }

    #[test]
    fn inject_returns_schema_unchanged_when_empty_bindings() {
        // bindings 空 → 原样返回
        let schema = weather_schema_with_city_required();
        let settings = serde_json::json!({"default_city": "北京"});
        let bindings = HashMap::new();

        let result = inject_plugin_settings(schema, Some(&settings), &bindings);
        let required = result.parameters["required"].as_array().unwrap();
        assert!(required.contains(&serde_json::json!("city")));
    }

    #[test]
    fn inject_removes_required_and_adds_default_when_setting_configured() {
        // 配了 default_city="北京" → city 从 required 移除 + 注入 default + 增强 description
        let schema = weather_schema_with_city_required();
        let settings = serde_json::json!({"default_city": "北京"});
        let mut bindings = HashMap::new();
        bindings.insert("city".to_string(), "default_city".to_string());

        let result = inject_plugin_settings(schema, Some(&settings), &bindings);

        // required 不再含 city（可能整个 required 字段被移除或变空数组）
        if let Some(req) = result.parameters.get("required").and_then(|r| r.as_array()) {
            assert!(!req.contains(&serde_json::json!("city")));
        }
        // city 参数注入了 default = "北京"
        assert_eq!(
            result.parameters["properties"]["city"]["default"],
            serde_json::json!("北京")
        );
        // description 追加了"（默认: 北京）"
        let desc = result.parameters["properties"]["city"]["description"]
            .as_str()
            .unwrap();
        assert!(
            desc.contains("（默认: 北京）"),
            "description 应含默认值增强: {desc}"
        );
    }

    #[test]
    fn inject_skips_null_and_empty_string_settings() {
        // default_city 是 null 或空字符串 → 视为未配置，不注入
        let schema = weather_schema_with_city_required();
        let mut bindings = HashMap::new();
        bindings.insert("city".to_string(), "default_city".to_string());

        // null
        let settings_null = serde_json::json!({"default_city": serde_json::Value::Null});
        let result = inject_plugin_settings(
            weather_schema_with_city_required(),
            Some(&settings_null),
            &bindings,
        );
        assert!(
            result.parameters["required"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("city"))
        );

        // 空字符串
        let settings_empty = serde_json::json!({"default_city": ""});
        let result = inject_plugin_settings(schema, Some(&settings_empty), &bindings);
        assert!(
            result.parameters["required"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("city"))
        );
    }

    #[test]
    fn inject_handles_multiple_bindings_simultaneously() {
        // 同时绑定 city→default_city 和 unit→temperature_unit
        let schema = weather_schema_with_city_required();
        let settings =
            serde_json::json!({"default_city": "上海", "temperature_unit": "fahrenheit"});
        let mut bindings = HashMap::new();
        bindings.insert("city".to_string(), "default_city".to_string());
        bindings.insert("unit".to_string(), "temperature_unit".to_string());

        let result = inject_plugin_settings(schema, Some(&settings), &bindings);

        // city: default=上海
        assert_eq!(
            result.parameters["properties"]["city"]["default"],
            serde_json::json!("上海")
        );
        // unit: default 被覆盖为 fahrenheit（原 default=celsius）
        assert_eq!(
            result.parameters["properties"]["unit"]["default"],
            serde_json::json!("fahrenheit")
        );
        // required 不含 city
        if let Some(req) = result.parameters.get("required").and_then(|r| r.as_array()) {
            assert!(!req.contains(&serde_json::json!("city")));
        }
    }

    #[test]
    fn inject_removes_required_field_when_becomes_empty() {
        // 只有一个 required 参数且被注入 → required 数组变空 → 整个 required 字段移除
        let schema = ActionSchema {
            name: "test".into(),
            description: "test".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "city": {"type": "string", "description": "城市"}
                },
                "required": ["city"]
            }),
        };
        let settings = serde_json::json!({"default_city": "深圳"});
        let mut bindings = HashMap::new();
        bindings.insert("city".to_string(), "default_city".to_string());

        let result = inject_plugin_settings(schema, Some(&settings), &bindings);
        // required 字段应被移除（变空数组时清理）
        assert!(
            result.parameters.get("required").is_none(),
            "required 变空应移除整个字段"
        );
    }

    #[test]
    fn inject_preserves_unbound_required_params() {
        // 有 city(required+bound) 和 text(required+unbound) → 只移除 city
        let schema = ActionSchema {
            name: "test".into(),
            description: "test".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "city": {"type": "string", "description": "城市"},
                    "text": {"type": "string", "description": "文本"}
                },
                "required": ["city", "text"]
            }),
        };
        let settings = serde_json::json!({"default_city": "杭州"});
        let mut bindings = HashMap::new();
        bindings.insert("city".to_string(), "default_city".to_string());

        let result = inject_plugin_settings(schema, Some(&settings), &bindings);
        let required = result.parameters["required"].as_array().unwrap();
        assert!(!required.contains(&serde_json::json!("city")));
        assert!(
            required.contains(&serde_json::json!("text")),
            "未绑定的 required 应保留"
        );
    }
}
