//! Capability Schema（0.9.7 §3.2）——能力描述的"身份证"。
//!
//! **纯 JSON Schema**（不绑 Rust 类型），让 0.11 派生 CLI（schema → clap 参数）
//! 和 MCP（schema → rmcp tool）零摩擦。
//!
//! 与 `ActionSchema` 结构相同但独立类型——Action 是交互动作的描述，
//! Capability 是纯能力的描述，语义不同不混用。
//!
//! `to_rig_tool()` 是唯一触碰 rig 类型的触点（与 `ActionSchema::to_rig_tool` 同模式），
//! rig breaking 只改这里，Capability trait 及全体实现零波及。

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// 能力描述 schema——纯 JSON Schema，是协议的"身份证"。
///
/// 三字段严格对齐 `rig::completion::ToolDefinition`——投影通过 `to_rig_tool()` 完成。
/// `to_rig_tool()` / （0.11）`to_mcp_tool()` / `to_clap()` 都从这份 schema 派生。
///
/// **0.11.2 §2.3**：新增 `sensitive: bool` 字段（default false）——声明敏感
/// （读隐私数据如应用列表/剪贴板历史），0.12 MCP server 暴露时需用户显式授权。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CapabilitySchema {
    /// 唯一标识（与 `Capability::id()` 一致）。传给模型时作为 tool name。
    pub name: String,
    /// 人类可读描述，直接送 LLM / CLI --help / MCP 描述。
    pub description: String,
    /// JSON Schema Object，遵循 draft-07。无参能力是 `{"type":"object","properties":{}}`。
    pub parameters: Value,
    /// 声明敏感（读隐私数据）——0.12 MCP server 暴露时需授权。default false。
    /// `search_apps` / `search_clipboard_history` 标 true。
    #[serde(default)]
    pub sensitive: bool,
}

impl Default for CapabilitySchema {
    /// 便利构造：`CapabilitySchema { name, description, parameters, ..Default::default() }`
    /// 让新增字段（如 sensitive）时已有构造点零改动。
    fn default() -> Self {
        CapabilitySchema {
            name: String::new(),
            description: String::new(),
            parameters: json!({ "type": "object", "properties": {} }),
            sensitive: false,
        }
    }
}

impl CapabilitySchema {
    /// 构造无参 schema（无参能力的 default）。
    #[allow(dead_code)] // Step 2 能力实现消费
    pub fn empty(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters: json!({ "type": "object", "properties": {} }),
            ..Default::default()
        }
    }

    /// 投影到 rig 的 `ToolDefinition`——AI 路由消费入口。
    ///
    /// 本方法是**唯一**触碰 rig 类型的地方（与 `ActionSchema::to_rig_tool` 同模式）；
    /// rig 若破坏 `ToolDefinition` 结构，只需要改这里。
    #[allow(dead_code)] // Step 4 接 AI tool 池时消费
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
        let s = CapabilitySchema::empty("capture_screen", "截取屏幕");
        assert_eq!(s.name, "capture_screen");
        assert_eq!(s.description, "截取屏幕");
        assert_eq!(s.parameters["type"], "object");
        assert!(s.parameters["properties"].is_object());
    }

    #[test]
    fn schema_with_parameters_preserves_them() {
        let s = CapabilitySchema {
            name: "search_files".into(),
            description: "搜索文件".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "搜索关键词" }
                },
                "required": ["query"]
            }),
            ..Default::default()
        };
        assert_eq!(s.parameters["properties"]["query"]["type"], "string");
        assert_eq!(s.parameters["required"][0], "query");
    }

    /// 投影后字段一一对应——AI tool 池消费的前提。
    #[test]
    fn schema_projects_to_rig_tool() {
        let s = CapabilitySchema {
            name: "translate".into(),
            description: "翻译文本".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string" }
                },
                "required": ["text"]
            }),
            ..Default::default()
        };
        let rig_def = s.to_rig_tool();
        assert_eq!(rig_def.name, "translate");
        assert_eq!(rig_def.description, "翻译文本");
        assert_eq!(rig_def.parameters["properties"]["text"]["type"], "string");
        assert_eq!(rig_def.parameters["required"][0], "text");
    }

    /// 协议层投影可行性：schema 能 round-trip 成纯 JSON。
    /// 0.11 CLI/MCP 派生从此 JSON 消费。
    #[test]
    fn schema_roundtrip_through_json() {
        let original = CapabilitySchema::empty("read_clipboard", "读剪贴板");
        let json_str = serde_json::to_string(&original).unwrap();
        let restored: CapabilitySchema = serde_json::from_str(&json_str).unwrap();
        assert_eq!(original, restored);
    }
}
