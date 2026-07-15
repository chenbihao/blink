//! 统一能力描述 Schema（0.9.0 §3.2）。
//!
//! 把 builtin / 插件 /(0.11) MCP / skill 四种能力源投影到同一份 tool 描述。
//! 对齐 **OpenAI function calling / MCP tool schema / rig `ToolDefinition`**。
//!
//! **中间层的目的**：Action trait 不直接依赖 rig 类型——rig 每月 breaking，
//! 中间层能把冲击面框在 `to_rig_tool()` 一个方法里。
//!
//! **danger_class 的位置**：不放在 schema 里，放在 `Action::danger_class()`。
//! 理由：danger 是"执行时是否弹确认"的属性，不是"传给模型的描述"的属性——
//! 送到 LLM 的 schema 不需要暴露内部安全等级。
//!
//! **0.9.0 定型，0.9.2 才真被 AI 路由消费**——现在只作为 Action trait 的能力自述。

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// 动作参数 JSON Schema（对齐 OpenAI function calling / MCP tool schema）。
///
/// 三字段严格对齐 `rig::completion::ToolDefinition`——投影通过 `to_rig_tool()`
/// 完成，不引入任何解释层。
///
/// **`allow(dead_code)`**:0.9.0 Phase 2 定义,通过 `Action::schema()` 由 12 个 builtin
/// 各自返回。0.9.1 起被 AI 路径消费(`to_rig_tool()` 送入 LLM)。
/// 中间态标记不影响运行时行为,0.9.2 移除。
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ActionSchema {
    /// 唯一标识（与 `Action::id()` 一致）。传给模型时作为 tool name。
    pub name: String,
    /// 人类可读描述，直接送入 LLM。空字符串合法（无参无描述动作，如系统命令）。
    pub description: String,
    /// JSON Schema Object，遵循 draft-07。无参动作是 `{"type":"object","properties":{}}`。
    pub parameters: Value,
}

impl ActionSchema {
    /// 构造无参 schema（12 个内置无参动作 / 未声明 parameters 的插件的 default）。
    #[allow(dead_code)]
    pub fn empty(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters: json!({ "type": "object", "properties": {} }),
        }
    }

    /// 投影到 rig 的 `ToolDefinition`——0.9.2 AI 路由消费入口。
    ///
    /// 本方法是**唯一**触碰 rig 类型的地方；rig 若破坏 `ToolDefinition` 结构，
    /// 只需要改这里，Action trait 及全体实现零波及。
    #[allow(dead_code)]
    pub fn to_rig_tool(&self) -> rig_core::completion::ToolDefinition {
        rig_core::completion::ToolDefinition {
            name: self.name.clone(),
            description: self.description.clone(),
            parameters: self.parameters.clone(),
        }
    }
}

/// 危险等级（0.9.0 §5.4 白名单铁则）。
///
/// **独立于交互模式**——主窗口 / Agent 窗口（0.10）共用这个枚举：
/// - `Safe`：可逆 / 只读 / 无副作用，AI 高置信可直接执行（Suggestion + Tab 或直接）
/// - `Dangerous`：不可逆 / 危险，**任何模式下都必须人机二次确认**（哪怕 Agent 窗口 tool loop）
///
/// **默认 `Safe`**——`Action` trait default impl 返回它；Dangerous 动作必须显式 override。
///
/// **`allow(dead_code)`**:0.9.0 Phase 2 定义,通过 `Action::danger_class()` 由 12 个 builtin
/// 显式返回。0.9.1 起被 AI 路径消费(§5.4 白名单铁则),0.9.2 移除 allow。
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DangerClass {
    /// 可逆 / 只读 / 无副作用：打开文件、翻译、查询、复制。
    Safe,
    /// 不可逆 / 危险：删除、发送、覆盖写、执行命令、关机、锁屏。
    Dangerous,
}

impl Default for DangerClass {
    fn default() -> Self {
        Self::Safe
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_schema_serializes_to_object_shape() {
        let s = ActionSchema::empty("open_settings", "打开设置");
        let json = serde_json::to_value(&s).unwrap();

        assert_eq!(json["name"], "open_settings");
        assert_eq!(json["description"], "打开设置");
        // OpenAI function calling / MCP 都要求 parameters 是一个 JSON Schema Object
        assert_eq!(json["parameters"]["type"], "object");
        assert!(json["parameters"]["properties"].is_object());
    }

    #[test]
    fn schema_projects_to_rig_tool_definition() {
        let s = ActionSchema {
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

        // rig::completion::ToolDefinition 字段与 ActionSchema 一一对应
        assert_eq!(rig_def.name, "open_url");
        assert_eq!(rig_def.description, "用默认浏览器打开一个 URL");
        assert_eq!(rig_def.parameters["properties"]["url"]["type"], "string");
        assert_eq!(rig_def.parameters["required"][0], "url");
    }

    #[test]
    fn danger_class_defaults_to_safe() {
        // Action trait default impl 依赖此语义；改了 default 会破坏 §5.4 白名单铁则
        assert_eq!(DangerClass::default(), DangerClass::Safe);
    }

    #[test]
    fn danger_class_serializes_stable() {
        // 用 externally tagged 的默认 serde 形式（"Safe" / "Dangerous"），
        // 配置文件 / IPC 消息里的 danger_class 字段稳定
        assert_eq!(
            serde_json::to_string(&DangerClass::Safe).unwrap(),
            "\"Safe\""
        );
        assert_eq!(
            serde_json::to_string(&DangerClass::Dangerous).unwrap(),
            "\"Dangerous\""
        );
    }

    #[test]
    fn schema_roundtrip_through_json() {
        // 用于跨 IPC 边界（0.9.2 前端渲染 tool call 候选面板会用到）
        let original = ActionSchema::empty("lock", "锁定电脑");
        let s = serde_json::to_string(&original).unwrap();
        let restored: ActionSchema = serde_json::from_str(&s).unwrap();
        assert_eq!(original, restored);
    }
}
