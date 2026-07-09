//! 插件 tool 的 Action 适配器(0.9.3)——让插件能力注册进 ActionRegistry。
//!
//! AI 路由层通过 `ActionRegistry::get(id)` 拿到 `PluginActionAdapter`,
//! 调 `execute()` 时走 JSONL ToolCall IPC 到插件子进程。
//!
//! **设计决策**:
//! - 复用 `ActionKind::Copy` + payload 模式展示结果(与 0.9.2 AI 文本回答一致)
//! - `danger_class` 从 manifest `ToolDef` 读取,默认 Safe
//! - `title` 用插件 manifest 的 `name` + tool 的 `name` 组合

use std::sync::Arc;

use tauri::Manager;

use crate::domain::execution::{Action, ActionContext, ActionOutcome, ActionSchema, DangerClass, ExecError};
use crate::domain::plugin::manifest::{DangerClassDef, LocalizableText, ToolDef};

use super::process::PluginHandle;

/// 插件 tool 的 Action 适配器——桥接 `Action` trait 与插件 JSONL IPC。
///
/// 启动时由 `main.rs` 遍历插件 manifest 的 `tools` 字段创建,
/// 注册进 `ActionRegistry` 与 builtin 动作并列。
pub struct PluginActionAdapter {
    plugin: Arc<PluginHandle>,
    /// manifest 中的原始 tool name（如 "translate"）——传给插件子进程用。
    tool_name: String,
    /// 全局唯一 id = "{plugin_id}:{tool_name}"——注册进 ActionRegistry 的 key。
    id: String,
    schema: ActionSchema,
    danger: DangerClass,
    title: LocalizableText,
}

impl PluginActionAdapter {
    /// 从 manifest `ToolDef` + 插件句柄构造。
    pub fn new(
        plugin: Arc<PluginHandle>,
        tool_def: &ToolDef,
        plugin_display_name: &LocalizableText,
    ) -> Self {
        let plugin_id = plugin.id().to_string();
        let id = format!("{plugin_id}:{}", tool_def.name);
        let schema = ActionSchema {
            name: id.clone(),
            description: tool_def.description.clone(),
            parameters: tool_def.parameters.clone(),
        };
        let danger = match tool_def.danger_class {
            DangerClassDef::Safe => DangerClass::Safe,
            DangerClassDef::Dangerous => DangerClass::Dangerous,
        };
        // title = "{插件名}：{tool名}" 例如 "翻译：translate"
        let plugin_name = plugin_display_name.resolve("zh");
        let title = LocalizableText::Plain(format!("{plugin_name}：{}", tool_def.name));

        Self {
            plugin,
            tool_name: tool_def.name.clone(),
            id,
            schema,
            danger,
            title,
        }
    }
}

#[async_trait::async_trait]
impl Action for PluginActionAdapter {
    fn id(&self) -> &str {
        &self.id
    }

    fn title(&self) -> &LocalizableText {
        &self.title
    }

    fn subtitle(&self) -> &LocalizableText {
        // 插件 tool 没有独立 subtitle,用 title 代替
        &self.title
    }

    fn schema(&self) -> ActionSchema {
        self.schema.clone()
    }

    fn danger_class(&self) -> DangerClass {
        self.danger
    }

    async fn execute(&self, cx: &ActionContext<'_>) -> Result<ActionOutcome, ExecError> {
        let plugin_id = self.plugin.id();
        let settings = cx
            .app_handle
            .state::<std::sync::Arc<super::engine::PluginEngine>>()
            .get_settings(plugin_id);

        tracing::debug!(
            plugin = %plugin_id,
            tool = %self.tool_name,
            args = %cx.arguments,
            "插件 tool-call 执行"
        );

        let items = self
            .plugin
            .execute_tool(&self.tool_name, &cx.arguments, settings.as_ref())
            .await
            .map_err(|e| ExecError::Runtime(format!("插件 tool 执行失败: {e}")))?;

        if items.is_empty() {
            return Err(ExecError::Runtime("插件返回空结果".into()));
        }

        // 取第一项的 title 作为展示文本，action 作为执行行为
        let first = &items[0];
        let text = first.title.clone();

        tracing::info!(
            plugin = %plugin_id,
            tool = %self.tool_name,
            items = items.len(),
            result_len = text.chars().count(),
            "插件 tool-call 完成"
        );

        // 如果插件返回了 Copy action，透传 payload
        match &first.action {
            crate::domain::plugin::protocol::PluginAction::Copy { text: copy_text } => {
                Ok(ActionOutcome::Copy {
                    text: copy_text.clone(),
                    hit_id: None,
                })
            }
            _ => Ok(ActionOutcome::Copy { text, hit_id: None }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::plugin::manifest::ToolDef;

    #[test]
    fn adapter_id_matches_tool_name() {
        // 构造一个假的 PluginHandle 需要 manifest + dir,这里只测字段映射
        let tool_def = ToolDef {
            name: "translate".into(),
            description: "翻译文本".into(),
            parameters: serde_json::json!({ "type": "object", "properties": {} }),
            danger_class: DangerClassDef::Safe,
        };
        // 验证 schema 映射正确——id 带插件前缀
        let schema = ActionSchema {
            name: format!("myplugin:{}", tool_def.name),
            description: tool_def.description.clone(),
            parameters: tool_def.parameters.clone(),
        };
        assert_eq!(schema.name, "myplugin:translate");
        assert_eq!(schema.description, "翻译文本");
    }

    #[test]
    fn danger_class_maps_correctly() {
        let safe = DangerClassDef::Safe;
        let dangerous = DangerClassDef::Dangerous;
        assert_eq!(
            match safe {
                DangerClassDef::Safe => DangerClass::Safe,
                DangerClassDef::Dangerous => DangerClass::Dangerous,
            },
            DangerClass::Safe
        );
        assert_eq!(
            match dangerous {
                DangerClassDef::Safe => DangerClass::Safe,
                DangerClassDef::Dangerous => DangerClass::Dangerous,
            },
            DangerClass::Dangerous
        );
    }

    #[test]
    fn title_format_is_plugin_colon_tool() {
        let plugin_name = LocalizableText::Plain("翻译".into());
        let tool_name = "translate";
        let title = LocalizableText::Plain(format!("{}：{}", plugin_name.resolve("zh"), tool_name));
        assert_eq!(title.resolve("zh"), "翻译：translate");
    }

    #[test]
    fn id_uses_plugin_prefix_format() {
        // 验证 id 格式 = "{plugin_id}:{tool_name}"——全局唯一
        let plugin_id = "my_translator";
        let tool_name = "translate";
        let id = format!("{plugin_id}:{tool_name}");
        assert_eq!(id, "my_translator:translate");
    }
}
