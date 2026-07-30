//! 统一 ToolSchema 公共基（0.14.6 §3.1）。
//!
//! `ActionSchema` 和 `CapabilitySchema` 共享同一份三字段结构 + 同一个 `to_rig_tool()`。
//! rig 触点从 2 处收敛到 1 处——rig 若 breaking `ToolDefinition`，只改这里。
//!
//! - `ActionSchema` = `ToolSchema` 的 type alias（0.14.6 后直接等价）
//! - `CapabilitySchema` = 扁平持有三字段 + `sensitive: bool`，`to_rig_tool()` 委托 `ToolSchema`
//!
//! 与 `DangerClass` 复用策略对齐——安全枚举也只有一份（在 `execution::schema`）。

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// 工具描述 Schema 公共基——对齐 OpenAI function calling / MCP tool schema / rig `ToolDefinition`。
///
/// 三字段严格对齐 `rig::completion::ToolDefinition`——投影通过 `to_rig_tool()` 完成，
/// 不引入任何解释层。
///
/// **`to_rig_tool()` 是全项目唯一触碰 rig 类型的地方**；rig 若破坏 `ToolDefinition` 结构，
/// 只需要改这里，`ActionSchema` / `CapabilitySchema` 及全体实现零波及。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolSchema {
    /// 唯一标识（与 `Action::id()` / `Capability::id()` 一致）。传给模型时作为 tool name。
    pub name: String,
    /// 人类可读描述，直接送入 LLM。空字符串合法（无参无描述动作，如系统命令）。
    pub description: String,
    /// JSON Schema Object，遵循 draft-07。无参动作是 `{"type":"object","properties":{}}`。
    pub parameters: Value,
}

impl Default for ToolSchema {
    fn default() -> Self {
        Self {
            name: String::new(),
            description: String::new(),
            parameters: json!({ "type": "object", "properties": {} }),
        }
    }
}

impl ToolSchema {
    /// 构造无参 schema（无参动作/能力的 default）。
    pub fn empty(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters: json!({ "type": "object", "properties": {} }),
        }
    }

    /// 投影到 rig 的 `ToolDefinition`——AI 路由消费入口。
    ///
    /// 本方法是**全项目唯一**触碰 rig 类型的地方；rig 若破坏 `ToolDefinition` 结构，
    /// 只需要改这里，`ActionSchema` / `CapabilitySchema` 及全体实现零波及。
    pub fn to_rig_tool(&self) -> rig_core::completion::ToolDefinition {
        rig_core::completion::ToolDefinition {
            name: self.name.clone(),
            description: self.description.clone(),
            parameters: self.parameters.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_schema_has_object_parameters() {
        let s = ToolSchema::empty("open_settings", "打开设置");
        assert_eq!(s.name, "open_settings");
        assert_eq!(s.description, "打开设置");
        assert_eq!(s.parameters["type"], "object");
        assert!(s.parameters["properties"].is_object());
    }

    #[test]
    fn schema_projects_to_rig_tool_definition() {
        let s = ToolSchema {
            name: "open_url".into(),
            description: "用默认浏览器打开一个 URL".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "要打开的 URL" }
                },
                "required": ["url"]
            }),
        };
        let rig_def = s.to_rig_tool();
        assert_eq!(rig_def.name, "open_url");
        assert_eq!(rig_def.description, "用默认浏览器打开一个 URL");
        assert_eq!(rig_def.parameters["properties"]["url"]["type"], "string");
        assert_eq!(rig_def.parameters["required"][0], "url");
    }

    #[test]
    fn schema_roundtrip_through_json() {
        let original = ToolSchema::empty("lock", "锁定电脑");
        let s = serde_json::to_string(&original).unwrap();
        let restored: ToolSchema = serde_json::from_str(&s).unwrap();
        assert_eq!(original, restored);
    }

    #[test]
    fn default_has_empty_object_parameters() {
        let d = ToolSchema::default();
        assert_eq!(d.name, "");
        assert_eq!(d.description, "");
        assert_eq!(d.parameters["type"], "object");
    }
}
