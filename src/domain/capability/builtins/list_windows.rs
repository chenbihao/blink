//! `list_windows` Capability（0.19.2）。
//!
//! 列出桌面上所有可见的顶层窗口 → `Items`。
//!
//! **背景**：`enumerate_pickable_windows()`（`infra/platform/window/list.rs`）已存在，
//! 返回 `Vec<PickableWindow>`（hwnd/x/y/w/h/title/process_name），但只被截图 overlay
//! 前端经 `screenshot_window_list` command 拉取做 hit-test，**未包装为 Capability**，
//! AI 完全看不到。本 cap 补上"AI 看到屏幕窗口布局"的感知入口，是"AI 截某 app"
//! "AI 把便签钉在某窗口旁"等所有定位场景的前置依赖。
//!
//! **与 `screenshot { op: window }` 的配合**（0.19.2）：AI 先调本 cap 拿到窗口列表
//! （含 hwnd），再调 `screenshot { op: "window", hwnd }` 截指定窗口。
//!
//! **sensitive=true**：读窗口列表属隐私敏感数据（窗口标题可能含敏感信息），
//! 与 `search_apps` 同级。
//!
//! **无 actions**：list_windows 是感知能力，不直接操作窗口。AI 拿到 hwnd 后
//! 组合其他 cap（如 `screenshot`）完成操作。
//!
//! **spawn_blocking**：`EnumWindows` 是同步 Win32 API（~5-15ms），按 spec-backend §一
//! "阻塞操作隔离"铁则，必须 `spawn_blocking` 挪出 tokio 工作线程，禁止在 async
//! 上下文裸跑。

use std::sync::Arc;

use serde_json::{Value, json};

use crate::domain::capability::{
    AiDefault, Capability, CapabilityError, CapabilityPolicy, CapabilityResult, CapabilitySchema,
    ConfirmationPolicy, DangerClass, InvokeContext, ItemResult, McpDefault, OriginSet,
    RuntimeRequirement,
};

/// `list_windows` — 列出桌面上所有可见的顶层窗口。
///
/// 入参：`{}`（无参）。
/// 出参：`Items`，每项 data 含 `{hwnd, title, process_name, x, y, w, h}`，
/// desc 为 `{title} ({process_name})`。
///
/// **返回顺序**：按 Z-order 从前景到背景（索引 0 = 最前景窗口）。
pub struct ListWindows;

#[async_trait::async_trait]
impl Capability for ListWindows {
    fn id(&self) -> &str {
        "list_windows"
    }

    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: "list_windows".into(),
            description: "列出桌面上所有可见的顶层窗口，返回每个窗口的句柄(hwnd)、标题、进程名和位置尺寸(x/y/w/h)。AI 可据此定位特定窗口，配合 screenshot 的 op:window 截取指定窗口。".into(),
            parameters: json!({
                "type": "object",
                "properties": {}
            }),
            sensitive: true, // 读窗口列表属隐私敏感数据（标题可能含敏感信息）
        }
    }

    fn policy(&self) -> CapabilityPolicy {
        CapabilityPolicy {
            allowed_origins: OriginSet::ALL,
            runtime_requirement: RuntimeRequirement::DESKTOP_SESSION,
            danger: DangerClass::Safe,
            sensitive: true,
            ai_default: AiDefault::On,
            mcp_default: McpDefault::DefaultOff,
            confirmation: ConfirmationPolicy::sensitive(),
        }
    }
    async fn invoke(
        &self,
        _args: Value,
        ctx: &InvokeContext<'_>,
    ) -> Result<CapabilityResult, CapabilityError> {
        // 铁则 1 前置检查
        if ctx.is_expired() {
            return Err(CapabilityError::Timeout {
                detail: "list_windows 截止时刻已过".into(),
            });
        }

        // spawn_blocking：EnumWindows 是同步 Win32 API（~5-15ms），
        // 按 spec-backend §一"阻塞操作隔离"铁则，不得在 async 上下文裸跑
        let windows =
            tokio::task::spawn_blocking(crate::infra::platform::window::enumerate_pickable_windows)
                .await
                .map_err(|e| CapabilityError::Internal {
                    detail: format!("list_windows task 崩溃: {e}"),
                })?;

        let results: Vec<ItemResult> = windows
            .into_iter()
            .map(|w| {
                let data = json!({
                    "hwnd": w.hwnd,
                    "title": w.title,
                    "process_name": w.process_name,
                    "x": w.x,
                    "y": w.y,
                    "w": w.w,
                    "h": w.h,
                });
                // desc: "{title} ({process_name})"——进程名为空时只显示标题
                let desc = if w.process_name.is_empty() {
                    w.title.clone()
                } else {
                    format!("{} ({})", w.title, w.process_name)
                };
                ItemResult {
                    data,
                    desc: Some(desc),
                    actions: vec![], // 感知能力，无直接操作
                }
            })
            .collect();

        tracing::debug!(count = results.len(), "list_windows 完成");
        Ok(CapabilityResult::Items { items: results })
    }
}

inventory::submit!(crate::domain::capability::CapabilityEntry {
    factory: || Arc::new(ListWindows) as Arc<dyn Capability>,
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_list_windows() {
        assert_eq!(ListWindows.id(), "list_windows");
    }

    #[test]
    fn schema_has_no_parameters() {
        let s = ListWindows.schema();
        assert_eq!(s.parameters["type"], "object");
        // 无 properties（空 object）
        assert!(s.parameters["properties"].as_object().unwrap().is_empty());
    }

    #[test]
    fn schema_sensitive_is_true() {
        let s = ListWindows.schema();
        assert!(s.sensitive, "list_windows 必须 sensitive=true");
    }

    #[test]
    fn schema_description_mentions_window() {
        let s = ListWindows.schema();
        assert!(
            s.description.contains("窗口"),
            "schema description 应提及窗口"
        );
        assert!(
            s.description.contains("hwnd"),
            "schema description 应提及 hwnd"
        );
    }
}
