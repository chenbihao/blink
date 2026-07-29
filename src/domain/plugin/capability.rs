//! 插件 tool 的 Capability 适配器（0.13.7）——让插件能力注册进 CapabilityRegistry。
//!
//! **演进背景**：0.9.3 起，插件 tool 通过 `PluginActionAdapter`（impl `Action`）注册进
//! `ActionRegistry`，靠 `ActionOutcome::Items` 借用 Capability 的 Items 模型。0.13.7
//! 收敛双体系——插件 tool 的语义本就是「纯计算→返回结果」（入参→出参，不碰 UI），
//! 天然属于 Capability 范畴。迁移后：
//! - 插件进 `CapabilityRegistry`，与 `search_files` / `read_clipboard` 等并列
//! - `ActionOutcome::Items` 变体删除，Action 回归纯粹（Copy/Open/Emit/Nop 副作用意图）
//! - 插件 `ToolDef.sensitive` 字段有处安放（`CapabilitySchema.sensitive`）
//!
//! AI 路由层通过 `CapabilityRegistry::get(id)` 拿到本 adapter，调 `invoke()` 时走
//! JSONL ToolCall IPC 到插件子进程，返回 `CapabilityResult::Items`。
//!
//! **危险操作**：`danger_class == Dangerous` 的插件 tool 仍可被 AI 调用，但
//! `CapabilityTool::call`（tool_adapter.rs）的 `check_dangerous_confirm` 会挂起等用户
//! 确认——弹窗在 ToolDyn 适配层，不进 `cap.invoke()`，不破坏 Capability「不碰 UI」铁则。

use std::sync::Arc;

use serde_json::Value;
use tauri::Manager;

use crate::domain::capability::{
    Capability, CapabilityError, CapabilityResult, CapabilitySchema, InvokeContext,
    ItemAction, ProjectionRule,
};
use crate::domain::execution::DangerClass;
use crate::domain::plugin::manifest::{DangerClassDef, ToolDef};

use super::process::PluginHandle;
use super::protocol::{PluginAction, PluginItem};

/// 构造插件 tool 的全局唯一 id。
///
/// 格式：`{plugin_id}_{tool_name}`，其中 plugin_id 中的 `.` 也替换为 `_`。
///
/// **为什么不用 `:` 分隔**：Anthropic / OpenAI 协议要求 tool name 匹配
/// `^[a-zA-Z0-9_-]+$`，`:` 和 `.` 均不合法，会导致 400 Bad Request。
/// 旧格式 `builtin.translate:translate` → 新格式 `builtin_translate_translate`。
///
/// 此函数是构造插件 tool id 的**唯一入口**——service.rs / commands.rs 等消费方
/// 必须调此函数而非自行拼接，保证 id 格式一致。
pub fn plugin_tool_id(plugin_id: &str, tool_name: &str) -> String {
    let sanitized = plugin_id.replace('.', "_");
    format!("{sanitized}_{tool_name}")
}

/// 插件 tool 的 Capability 适配器——桥接 `Capability` trait 与插件 JSONL IPC。
///
/// 启动时由 `main.rs` 遍历插件 manifest 的 `tools` 字段创建，
/// 注册进 `CapabilityRegistry` 与 builtin 能力（search_files 等）并列。
pub struct PluginCapabilityAdapter {
    plugin: Arc<PluginHandle>,
    /// manifest 中的原始 tool name（如 "translate"）——传给插件子进程用。
    tool_name: String,
    /// 全局唯一 id = `plugin_tool_id(plugin_id, tool_name)`——注册进 CapabilityRegistry 的 key。
    /// 仅含 `[a-zA-Z0-9_]`，满足 Anthropic / OpenAI tool name 正则 `^[a-zA-Z0-9_-]+$`。
    id: String,
    schema: CapabilitySchema,
    danger: DangerClass,
    /// manifest 投影规则（0.14.3）。Some → 轨道 A（插件返回纯 data，core 投影）；
    /// None → 轨道 B（插件返回 PluginItem，core 用 `plugin_item_to_item_result` 映射）。
    projection: Option<ProjectionRule>,
}

impl PluginCapabilityAdapter {
    /// 从 manifest `ToolDef` + 插件句柄构造。
    pub fn new(plugin: Arc<PluginHandle>, tool_def: &ToolDef) -> Self {
        let plugin_id = plugin.id().to_string();
        let id = plugin_tool_id(&plugin_id, &tool_def.name);
        let schema = CapabilitySchema {
            name: id.clone(),
            description: tool_def.description.clone(),
            parameters: tool_def.parameters.clone(),
            sensitive: tool_def.sensitive,
        };
        let danger = match tool_def.danger_class {
            DangerClassDef::Safe => DangerClass::Safe,
            DangerClassDef::Dangerous => DangerClass::Dangerous,
        };

        Self {
            plugin,
            tool_name: tool_def.name.clone(),
            id,
            schema,
            danger,
            projection: tool_def.projection.clone(),
        }
    }
}

#[async_trait::async_trait]
impl Capability for PluginCapabilityAdapter {
    fn id(&self) -> &str {
        &self.id
    }

    fn schema(&self) -> CapabilitySchema {
        self.schema.clone()
    }

    fn danger_class(&self) -> DangerClass {
        self.danger
    }

    async fn invoke(
        &self,
        args: Value,
        ctx: &InvokeContext<'_>,
    ) -> Result<CapabilityResult, CapabilityError> {
        let plugin_id = self.plugin.id();
        let settings = ctx
            .app_handle
            .state::<std::sync::Arc<super::engine::PluginEngine>>()
            .get_settings(plugin_id);

        tracing::debug!(
            plugin = %plugin_id,
            tool = %self.tool_name,
            args = %args,
            "插件 tool-call 执行（Capability 路径）"
        );

        // 铁则 1 前置检查：deadline 已过则不启动插件进程
        if ctx.is_expired() {
            return Err(CapabilityError::Timeout {
                detail: format!("插件 tool {} 截止时刻已过，不启动", self.tool_name),
            });
        }

        // 0.14.3: manifest 配了 projection → 轨道 A（纯 data + 投影引擎）
        if let Some(projection) = &self.projection {
            let raw = self
                .plugin
                .execute_tool_raw(&self.tool_name, &args, settings.as_ref())
                .await
                .map_err(|e| CapabilityError::Internal {
                    detail: format!("插件 tool 执行失败: {e}"),
                })?;

            tracing::info!(
                plugin = %plugin_id,
                tool = %self.tool_name,
                "插件 tool-call 完成（轨道 A 纯数据）"
            );

            return Ok(crate::domain::capability::project(&raw.data, projection));
        }

        // 轨道 B（旧协议）：插件返回 PluginItem 列表，用 plugin_item_to_item_result 映射
        let items = self
            .plugin
            .execute_tool(&self.tool_name, &args, settings.as_ref())
            .await
            .map_err(|e| CapabilityError::Internal {
                detail: format!("插件 tool 执行失败: {e}"),
            })?;

        if items.is_empty() {
            return Err(CapabilityError::Internal {
                detail: "插件返回空结果".into(),
            });
        }

        // 投影全量 PluginItem → ItemResult（不截断，保留全量结果给 AI/前端）
        let results: Vec<crate::domain::capability::ItemResult> =
            items.iter().map(plugin_item_to_item_result).collect();

        tracing::info!(
            plugin = %plugin_id,
            tool = %self.tool_name,
            items = items.len(),
            "插件 tool-call 完成（全量结果）"
        );

        Ok(CapabilityResult::Items { items: results })
    }
}

/// PluginItem → ItemResult 投影（0.11.0 改进 1，0.13.7 从 action.rs 迁入，0.14 适配新结构）。
///
/// **0.14 变化**：`ItemResult` 从 `{ title, subtitle, payload, score }` 改为
/// `{ data, desc, actions }`。此函数做过渡映射：
/// - `data` ← `payload`（优先取 `PluginItem.payload`；缺失时从 `action` 提取兜底）
/// - `desc` ← `subtitle`
/// - `actions` ← 从 `PluginAction` 派生（Copy→Copy, Open→OpenFile, None→空）
///
/// **0.14.3 插件迁移后**：此函数将被投影引擎（`project()`）取代——
/// 插件返回 `PluginRawResult { data }`，manifest 配 `ProjectionRule` 投影。
pub(crate) fn plugin_item_to_item_result(
    item: &PluginItem,
) -> crate::domain::capability::ItemResult {
    let data = item.payload.clone().unwrap_or_else(|| match &item.action {
        PluginAction::Copy { text } => serde_json::json!({ "text": text }),
        PluginAction::Open { path } => serde_json::json!({ "path": path }),
        PluginAction::None => serde_json::json!({}),
    });

    let actions = match &item.action {
        PluginAction::Copy { .. } => vec![ItemAction::Copy { pointer: None }],
        PluginAction::Open { .. } => vec![ItemAction::OpenFile { pointer: None }],
        PluginAction::None => vec![],
    };

    crate::domain::capability::ItemResult {
        data,
        desc: item.subtitle.clone(),
        actions,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::plugin::manifest::ToolDef;
    use crate::domain::plugin::protocol::{PluginAction, PluginItem};

    #[test]
    fn adapter_id_matches_tool_name() {
        // 验证 schema 映射正确——id 带 plugin_id 前缀（下划线分隔）
        let tool_def = ToolDef {
            name: "translate".into(),
            description: "翻译文本".into(),
            parameters: serde_json::json!({ "type": "object", "properties": {} }),
            danger_class: DangerClassDef::Safe,
            ..Default::default()
        };
        let id = plugin_tool_id("myplugin", &tool_def.name);
        assert_eq!(id, "myplugin_translate");
        assert_eq!(tool_def.description, "翻译文本");
    }

    #[test]
    fn danger_class_maps_correctly() {
        assert_eq!(
            match DangerClassDef::Safe {
                DangerClassDef::Safe => DangerClass::Safe,
                DangerClassDef::Dangerous => DangerClass::Dangerous,
            },
            DangerClass::Safe
        );
        assert_eq!(
            match DangerClassDef::Dangerous {
                DangerClassDef::Safe => DangerClass::Safe,
                DangerClassDef::Dangerous => DangerClass::Dangerous,
            },
            DangerClass::Dangerous
        );
    }

    #[test]
    fn id_uses_plugin_prefix_format() {
        // 验证 id 格式 = plugin_tool_id(plugin_id, tool_name)——全局唯一
        // 且仅含 [a-zA-Z0-9_]，满足 Anthropic/OpenAI tool name 正则 ^[a-zA-Z0-9_-]+$
        let id = plugin_tool_id("my_translator", "translate");
        assert_eq!(id, "my_translator_translate");
    }

    #[test]
    fn plugin_tool_id_sanitizes_dots() {
        // builtin.translate:translate → builtin_translate_translate
        let id = plugin_tool_id("builtin.translate", "translate");
        assert_eq!(id, "builtin_translate_translate");
        assert!(id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'));
    }

    // ── plugin_item_to_item_result 投影（从 action.rs 迁移）──────────────────

    #[test]
    fn projection_uses_payload_when_present() {
        // 插件填了 payload → data 优先用，忽略 action 兜底
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
        // data 优先用插件填的 payload
        assert_eq!(r.data["ip"], "1.2.3.4");
        assert_eq!(r.data["type"], "public");
        // desc ← subtitle
        assert_eq!(r.desc.as_deref(), Some("北京"));
        // Copy action → Copy ItemAction
        assert_eq!(r.actions.len(), 1);
        assert!(matches!(r.actions[0], ItemAction::Copy { .. }));
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
        assert_eq!(r.data["text"], "192.168.1.5");
        assert!(matches!(r.actions[0], ItemAction::Copy { .. }));
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
        assert_eq!(r.data["path"], "C:\\code.lnk");
        assert!(matches!(r.actions[0], ItemAction::OpenFile { .. }));
    }

    #[test]
    fn projection_none_action_yields_empty_payload() {
        // 无 payload + None action → 兜底 {}，空 actions
        let item = PluginItem {
            title: "提示项".into(),
            score: 0.5,
            action: PluginAction::None,
            ..Default::default()
        };
        let r = plugin_item_to_item_result(&item);
        assert!(r.data.as_object().unwrap().is_empty());
        assert!(r.actions.is_empty());
    }
}
