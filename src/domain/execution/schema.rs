//! Execution 域 Schema（0.14.6 §3.1 收敛后）。
//!
//! `ActionSchema` 已收敛为 `ToolSchema` 的 type alias——三字段 + `to_rig_tool()`
//! 统一在 `domain::schema::ToolSchema`，rig 触点从 2 处收敛到 1 处。
//!
//! `DangerClass` 保留在此（安全枚举只有一份，Capability 复用它）。

use serde::{Deserialize, Serialize};

/// 动作参数 Schema = `ToolSchema`（0.14.6 §3.1 合并）。
///
/// type alias 让所有 `ActionSchema { name, description, parameters }` 构造点零改动。
/// `to_rig_tool()` / `empty()` / `Default` 全部来自 `ToolSchema`。
pub type ActionSchema = crate::domain::schema::ToolSchema;

/// 危险等级（0.9.0 §5.4 白名单铁则）。
///
/// **独立于交互模式**——主窗口 / Agent 窗口（0.10）共用这个枚举：
/// - `Safe`：可逆 / 只读 / 无副作用，AI 高置信可直接执行（Suggestion + Tab 或直接）
/// - `Dangerous`：不可逆 / 危险，**任何模式下都必须人机二次确认**（哪怕 Agent 窗口 tool loop）
///
/// **默认 `Safe`**——`Action` trait default impl 返回它；Dangerous 动作必须显式 override。
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
    use serde_json::json;

    #[test]
    fn empty_schema_serializes_to_object_shape() {
        let s = ActionSchema::empty("open_settings", "打开设置");
        let json = serde_json::to_value(&s).unwrap();

        assert_eq!(json["name"], "open_settings");
        assert_eq!(json["description"], "打开设置");
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

        assert_eq!(rig_def.name, "open_url");
        assert_eq!(rig_def.description, "用默认浏览器打开一个 URL");
        assert_eq!(rig_def.parameters["properties"]["url"]["type"], "string");
        assert_eq!(rig_def.parameters["required"][0], "url");
    }

    #[test]
    fn danger_class_defaults_to_safe() {
        assert_eq!(DangerClass::default(), DangerClass::Safe);
    }

    #[test]
    fn danger_class_serializes_stable() {
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
        let original = ActionSchema::empty("lock", "锁定电脑");
        let s = serde_json::to_string(&original).unwrap();
        let restored: ActionSchema = serde_json::from_str(&s).unwrap();
        assert_eq!(original, restored);
    }
}
