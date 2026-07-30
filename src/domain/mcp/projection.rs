//! MCP 正向投影（0.13.4）——`CapabilitySchema → rmcp::model::Tool`。
//!
//! 0.13.0 建了反向投影（`rmcp::model::Tool → CapabilitySchema`，用于 MCP client），
//! 0.13.4 补正向投影——把 Blink 的 Capability 暴露给外部 MCP client。
//!
//! 两方向投影共用 `src/domain/mcp/projection.rs`，是一对对称的开放能力：
//! - 反向投影（0.13.0）：消费外部 tool，`rmcp::model::Tool::to_capability_schema()`
//! - 正向投影（0.13.4）：暴露自身能力，`CapabilitySchema::to_mcp_tool()`
//!
//! **为什么不在 CapabilitySchema 上直接 impl**：
//! CapabilitySchema 在 `domain/capability/` 层，不依赖 rmcp。
//! 投影函数建在 `domain/mcp/` 层，保持 capability 层零 rmcp 耦合。

use crate::domain::capability::CapabilitySchema;
use rmcp::model::Tool;
use std::sync::Arc;

/// 正向投影：`CapabilitySchema → rmcp::model::Tool`。
///
/// 把 Blink 的能力 schema 转换为 MCP 协议的 Tool 定义，
/// 供外部 MCP client 拉取 tool 列表时消费。
///
/// **字段映射**：
/// - `name` → `Tool.name`
/// - `description` → `Tool.description`
/// - `parameters`（JSON Schema）→ `Tool.input_schema`（JsonObject）
///
/// **sensitive 不映射到 MCP annotations**：
/// MCP annotations 是给 client UI 的提示（如 `readOnlyHint`），
/// sensitive 是 Blink 的授权标记（需弹确认卡片），语义不同。
/// sensitive 在 `call_tool` 时由 `BlinkMcpServer` 检查并触发授权。
pub fn capability_schema_to_mcp_tool(schema: &CapabilitySchema) -> Tool {
    // parameters 是 JSON Schema Object，转成 rmcp 的 JsonObject
    let input_schema = if schema.parameters.is_object() {
        schema.parameters.as_object().cloned().unwrap_or_default()
    } else {
        // 非对象的 parameters（不应该出现），用空 object 兜底
        serde_json::Map::new()
    };

    // rmcp::model::Tool 是 #[non_exhaustive]，必须用 Tool::new 构造
    Tool::new(
        schema.name.clone(),
        schema.description.clone(),
        Arc::new(input_schema),
    )
}

/// 批量正向投影：`Vec<CapabilitySchema> → Vec<rmcp::model::Tool>`。
pub fn capability_schemas_to_mcp_tools(schemas: &[CapabilitySchema]) -> Vec<Tool> {
    schemas.iter().map(capability_schema_to_mcp_tool).collect()
}

// ── 单测 ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn projection_maps_name_and_description() {
        let schema = CapabilitySchema {
            name: "capture_screen".into(),
            description: "截取屏幕截图".into(),
            parameters: json!({"type": "object", "properties": {}}),
            sensitive: false,
        };
        let tool = capability_schema_to_mcp_tool(&schema);
        assert_eq!(tool.name, "capture_screen");
        assert_eq!(tool.description.as_deref(), Some("截取屏幕截图"));
    }

    #[test]
    fn projection_maps_input_schema() {
        let schema = CapabilitySchema {
            name: "search_files".into(),
            description: "搜索文件".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "搜索关键词" }
                },
                "required": ["query"]
            }),
            sensitive: false,
        };
        let tool = capability_schema_to_mcp_tool(&schema);
        assert_eq!(tool.input_schema["type"], "object");
        assert_eq!(tool.input_schema["properties"]["query"]["type"], "string");
        assert_eq!(tool.input_schema["required"][0], "query");
    }

    #[test]
    fn projection_handles_empty_parameters() {
        let schema = CapabilitySchema::empty("read_clipboard", "读剪贴板");
        let tool = capability_schema_to_mcp_tool(&schema);
        assert_eq!(tool.name, "read_clipboard");
        // CapabilitySchema::empty 的 parameters 是 {"type":"object","properties":{}}，
        // 投影后 input_schema 应包含这两个 key
        assert!(!tool.input_schema.is_empty());
        assert_eq!(tool.input_schema["type"], "object");
    }

    #[test]
    fn projection_does_not_set_annotations_for_sensitive() {
        // sensitive 不映射到 MCP annotations——授权由 BlinkMcpServer 在 call_tool 时检查
        let schema = CapabilitySchema {
            name: "search_apps".into(),
            description: "搜索应用".into(),
            parameters: json!({"type": "object", "properties": {}}),
            sensitive: true,
        };
        let tool = capability_schema_to_mcp_tool(&schema);
        assert!(tool.annotations.is_none());
    }

    #[test]
    fn batch_projection_preserves_count() {
        let schemas = vec![
            CapabilitySchema::empty("cap_a", "A"),
            CapabilitySchema::empty("cap_b", "B"),
            CapabilitySchema::empty("cap_c", "C"),
        ];
        let tools = capability_schemas_to_mcp_tools(&schemas);
        assert_eq!(tools.len(), 3);
        assert_eq!(tools[0].name, "cap_a");
        assert_eq!(tools[2].name, "cap_c");
    }

    #[test]
    fn projection_handles_non_object_parameters_gracefully() {
        // 不应该出现，但要优雅降级
        let schema = CapabilitySchema {
            name: "weird".into(),
            description: "非 object 参数".into(),
            parameters: json!("not an object"),
            sensitive: false,
        };
        let tool = capability_schema_to_mcp_tool(&schema);
        assert_eq!(tool.name, "weird");
        // 非 object → 空 JsonObject 兜底（Arc<Map>，检查 is_empty）
        assert!(tool.input_schema.is_empty());
    }
}
