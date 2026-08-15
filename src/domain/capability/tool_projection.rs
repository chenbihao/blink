//! AI Capability schema 投影与插件 settings 注入。
//!
//! 从 `execution::group` 迁入（0.21.7）——execution 模块删除后，AI tool 池构建
//! 和插件参数注入逻辑留在 capability 域，使用 `ToolSchema` 公共基。
//!
//! `build_capability_tools` 从 `CapabilityRegistry` 构建工具列表；
//! `inject_plugin_settings` 在 manifest `setting_bindings` 声明时动态注入
//! 插件配置值作为参数 default。

use crate::domain::capability::CapabilityRegistry;
use crate::domain::schema::ToolSchema;

/// 构建 AI tool 列表。
///
/// 从 `CapabilityRegistry` 的全量 `list()` 投影为 `ToolSchema` 列表。
/// `CapabilitySchema` 与 `ToolSchema` 结构相同（name/description/parameters），
/// 直接字段投影；`sensitive` 只参与执行前确认，不发送给模型。
pub fn build_capability_tools(cap_registry: &CapabilityRegistry) -> Vec<ToolSchema> {
    cap_registry
        .list()
        .into_iter()
        .map(|cap_schema| ToolSchema {
            name: cap_schema.name,
            description: cap_schema.description,
            parameters: cap_schema.parameters,
        })
        .collect()
}

/// 参数 schema 动态注入插件 settings（0.11.1 §2.3b）。
///
/// **投影层改动**：不改 manifest 的 `parameters` 原文，生成新的 `ToolSchema`。
/// 调用方（`service.rs` AI lane）每次 AI 请求时从 `PluginEngine` 取 settings +
/// 从 manifest `ToolDef.setting_bindings` 取绑定映射，调此函数注入。
///
/// **注入规则**（文档 §2.3b）：对 `bindings` 中每个 `(param_name, setting_key)`：
/// 1. 从 settings 读 `setting_key` 的值；跳过 null / 空字符串
/// 2. **required → optional**：从 `parameters.required` 移除 `param_name`
/// 3. **注入 default**：`parameters.properties[param_name].default = <setting_value>`
/// 4. **description 增强**：追加"（默认: {value}）"
pub fn inject_plugin_settings(
    mut schema: ToolSchema,
    settings: Option<&serde_json::Value>,
    bindings: &std::collections::HashMap<String, String>,
) -> ToolSchema {
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
    fn ai_tool_list_is_capability_only() {
        let cap_registry = CapabilityRegistry::new();
        let expected: std::collections::HashSet<String> = cap_registry
            .list()
            .into_iter()
            .map(|schema| schema.name)
            .collect();
        let actual: std::collections::HashSet<String> = build_capability_tools(&cap_registry)
            .into_iter()
            .map(|schema| schema.name)
            .collect();

        assert_eq!(actual, expected);
        assert!(!actual.contains("system_action"));
        assert!(!actual.contains("blink_action"));
        // 0.21.1: lock/shutdown 等已迁为 Capability，现在会出现在列表中
        assert!(actual.contains("lock"));
        assert!(actual.contains("shutdown"));
    }

    // ── 0.11.1 §2.3b：inject_plugin_settings 参数动态注入 ──────────────────

    use std::collections::HashMap;

    fn weather_schema_with_city_required() -> ToolSchema {
        // 模拟 weather 插件的原始 schema（city 是 required）
        ToolSchema {
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
        let schema = ToolSchema {
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
        let schema = ToolSchema {
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
