//! `list_windows` Capability（0.19.2 + 0.22.14）。
//!
//! 列出桌面上所有可见的顶层窗口 → `Items`。
//!
//! **背景**：`enumerate_pickable_windows()`（`infra/platform/window/list.rs`）已存在，
//! 返回 `Vec<PickableWindow>`（hwnd/x/y/w/h/title/process_name），但只被截图 overlay
//! 前端经 `screenshot_window_list` command 拉取做 hit-test，**未包装为 Capability**，
//! AI 完全看不到。本 cap 补上"AI 看到屏幕窗口布局"的感知入口，是"AI 截某 app"
//! "AI 把便签钉在某窗口旁"等所有定位场景的前置依赖。
//!
//! **与 `screenshot { op: window }` 的配合**：AI 先调本 cap 拿到窗口列表
//! （含 `window_ref`），再调 `screenshot { op: "window", window_ref }` 截指定窗口。
//!
//! **0.22.14 变更**：返回数据中不再暴露裸 `hwnd`，改为返回 `window_ref`（opaque
//! 短期引用）。`window_ref` 绑定 HWND/PID/Blink 身份/generation，在 `screenshot`
//! 和 `manage_window` 中必须通过 `window_ref` 操作窗口，防止 AI 使用过期的
//! 或捏造的 HWND。兼容路径仍保留裸 `hwnd`（仅 `LocalCommand` / 内部协议）。
//!
//! **sensitive=true**：读窗口列表属隐私敏感数据（窗口标题可能含敏感信息），
//! 与 `search_apps` 同级。
//!
//! **无 actions**：list_windows 是感知能力，不直接操作窗口。AI 拿到 `window_ref` 后
//! 组合其他 cap（如 `screenshot`、`manage_window`）完成操作。
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
            description: "列出桌面上所有可见的顶层窗口，返回每个窗口的安全引用(window_ref)、标题、进程名、是否为 Blink 窗口(is_blink)和位置尺寸(x/y/w/h)。AI 应使用 window_ref 配合 screenshot 的 op:window 截取指定窗口，或用 manage_window 操作窗口。window_ref 是短期有效的安全引用，过期后需重新调用本能力获取新引用。".into(),
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

        // 0.22.14：推进 generation，注册 window_ref
        let gen_val = crate::infra::platform::window::next_generation();
        let current_pid = unsafe { windows::Win32::System::Threading::GetCurrentProcessId() };

        // 清理旧 generation 的引用（防止注册表无限增长）
        crate::infra::platform::window::cleanup_old_refs();

        let results: Vec<ItemResult> = windows
            .into_iter()
            .map(|w| {
                // 判断是否 Blink 窗口
                let hwnd_raw = windows::Win32::Foundation::HWND(w.hwnd as *mut _);
                let pid = crate::infra::platform::window::get_window_pid(hwnd_raw);
                let is_blink = pid == current_pid;

                // 注册 opaque window_ref（CSPRNG 失败时结构化错误传播）
                let window_ref = crate::infra::platform::window::register_window_ref(
                    w.hwnd,
                    pid,
                    is_blink,
                    &w.title,
                    &w.process_name,
                    gen_val,
                )
                .map_err(|e| CapabilityError::Internal {
                    detail: format!("window_ref 注册失败: {e}"),
                })?;

                let data = json!({
                    "window_ref": window_ref,
                    "is_blink": is_blink,
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
                Ok(ItemResult {
                    data,
                    desc: Some(desc),
                    actions: vec![], // 感知能力，无直接操作
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        tracing::debug!(
            count = results.len(),
            generation = gen_val,
            "list_windows 完成"
        );
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
            s.description.contains("window_ref"),
            "schema description 应提及 window_ref"
        );
    }

    #[test]
    fn schema_description_mentions_blink() {
        let s = ListWindows.schema();
        assert!(
            s.description.contains("is_blink"),
            "schema description 应提及 is_blink"
        );
    }

    #[test]
    fn schema_description_mentions_manage_window() {
        let s = ListWindows.schema();
        assert!(
            s.description.contains("manage_window"),
            "schema description 应提及 manage_window"
        );
    }
}
