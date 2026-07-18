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

use crate::domain::execution::{
    Action, ActionContext, ActionOutcome, ActionSchema, DangerClass, ExecError,
};
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

        // 0.11.0 改进 1: 不截断——投影全量 PluginItem → ItemResult，返回 ActionOutcome::Items。
        // 老实现只取 items[0] 导致"查 IP 只拿到局域网值"；现全量保留，
        // 消费方（handle_ai_tool_calls）走统一投影路径（items_to_entries）。
        let results: Vec<crate::domain::capability::ItemResult> =
            items.iter().map(plugin_item_to_item_result).collect();

        tracing::info!(
            plugin = %plugin_id,
            tool = %self.tool_name,
            items = items.len(),
            "插件 tool-call 完成（全量结果）"
        );

        Ok(ActionOutcome::Items { items: results })
    }
}

/// PluginItem → ItemResult 投影（0.11.0 改进 1）。
///
/// **投影规则**（文档 §2.1）：
/// - `payload` 优先取 `PluginItem.payload`（新字段，结构化数据给 AI）；
///   缺失时（老插件）从 `action` 提取兜底——`Copy→{"text":...}`, `Open→{"path":...}`, `None→{}`
/// - `title`/`subtitle`/`score` 直接映射
///
/// 兜底逻辑保证老插件（不填 payload）也能正常工作——AI 从 payload 读到结构化数据，
/// 而非从展示文本反推。新插件应在 tool-call 路径下主动填 payload。
fn plugin_item_to_item_result(
    item: &super::protocol::PluginItem,
) -> crate::domain::capability::ItemResult {
    use super::protocol::PluginAction;

    let payload = item.payload.clone().unwrap_or_else(|| match &item.action {
        PluginAction::Copy { text } => serde_json::json!({ "text": text }),
        PluginAction::Open { path } => serde_json::json!({ "path": path }),
        PluginAction::None => serde_json::json!({}),
    });

    crate::domain::capability::ItemResult {
        title: item.title.clone(),
        subtitle: item.subtitle.clone(),
        payload,
        score: Some(item.score),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::plugin::manifest::ToolDef;
    use crate::domain::plugin::protocol::{PluginAction, PluginItem};

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

    // ── 0.11.0 改进 1: plugin_item_to_item_result 投影 ────────────────────────

    #[test]
    fn projection_uses_payload_when_present() {
        // 插件填了 payload → 优先用，忽略 action 兜底
        let item = PluginItem {
            title: "公网 IP: 1.2.3.4".into(),
            subtitle: Some("北京".into()),
            score: 0.9,
            action: PluginAction::Copy {
                text: "1.2.3.4".into(),
            },
            payload: Some(serde_json::json!({ "ip": "1.2.3.4", "type": "public" })),
            ..Default::default()
        };
        let r = plugin_item_to_item_result(&item);
        assert_eq!(r.title, "公网 IP: 1.2.3.4");
        assert_eq!(r.subtitle.as_deref(), Some("北京"));
        assert_eq!(r.score, Some(0.9));
        // payload 优先用插件填的，不从 action 兜底
        assert_eq!(r.payload["ip"], "1.2.3.4");
        assert_eq!(r.payload["type"], "public");
    }

    #[test]
    fn projection_falls_back_to_copy_action_text() {
        // 老插件无 payload + Copy action → 兜底 {"text": ...}
        let item = PluginItem {
            title: "本地 IP: 192.168.1.5".into(),
            score: 1.0,
            action: PluginAction::Copy {
                text: "192.168.1.5".into(),
            },
            ..Default::default()
        };
        let r = plugin_item_to_item_result(&item);
        assert_eq!(r.payload["text"], "192.168.1.5");
    }

    #[test]
    fn projection_falls_back_to_open_action_path() {
        // 老插件无 payload + Open action → 兜底 {"path": ...}
        let item = PluginItem {
            title: "VSCode".into(),
            score: 0.95,
            action: PluginAction::Open {
                path: "C:\\code.lnk".into(),
            },
            ..Default::default()
        };
        let r = plugin_item_to_item_result(&item);
        assert_eq!(r.payload["path"], "C:\\code.lnk");
    }

    #[test]
    fn projection_none_action_yields_empty_payload() {
        // 无 payload + None action → 兜底 {}
        let item = PluginItem {
            title: "提示项".into(),
            score: 0.5,
            action: PluginAction::None,
            ..Default::default()
        };
        let r = plugin_item_to_item_result(&item);
        assert!(r.payload.as_object().unwrap().is_empty());
    }
}
