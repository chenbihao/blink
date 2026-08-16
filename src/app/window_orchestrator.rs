//! 窗口编排层（0.21.14）——从 `infra/platform/window/windows.rs` 上移的业务编排。
//!
//! **职责**：Tauri managed-state 定位领域服务 + 采集上下文 + 调 infra primitive。
//! `windows.rs` 只保留参数化 Win32 / Tauri 窗口 primitive，不反向依赖 domain。
//!
//! **关键函数**：
//! - `invoke()`：热键唤起编排（采集快照 → 更新 SearchService → show_main_window → 异步选区）
//! - `hide_chat_window()`：隐藏 chat 窗口编排（abort_active → hide_chat_window_primitive）

use std::sync::Arc;

use tauri::{AppHandle, Emitter, Manager};

use crate::domain::config::ContextConfig;
use crate::domain::event_names::EventNames;
use crate::domain::search::SearchService;
use crate::infra::platform::context::CollectParams;
use crate::infra::platform::window;

/// 唤起编排：采集上下文快照 → 更新 SearchService → show 主窗口 → 异步提取选区。
///
/// **采集时机很重要**：必须在 show() 之前调用，否则拿到的前台是 Blink 自己。
///
/// 0.21.14：从 `infra/platform/window/windows.rs::invoke` 上移。
/// infra 层 `show_main_window` 只负责定位 + show + focus + emit SHOWN，
/// 本函数负责 domain state 定位 + context 采集 + SearchService 更新 + 异步选区回填。
pub fn invoke(app: &AppHandle) {
    // 1. 先采集上下文快照（show 之前！）
    //    读内存 ContextConfig（零 IO，热键回调不能 await），按配置过滤采集
    let context_cfg = app
        .try_state::<Arc<std::sync::RwLock<ContextConfig>>>()
        .map(|c| c.read().unwrap().clone())
        .unwrap_or_default();
    let collect_params = CollectParams {
        enabled: context_cfg.enabled,
        clipboard_enabled: context_cfg.clipboard_enabled,
        sensitive_apps: context_cfg.sensitive_apps.clone(),
    };
    let snapshot = crate::infra::platform::context::collect(&collect_params);
    if let Some(hwnd) = snapshot
        .foreground_app
        .as_ref()
        .map(|foreground| foreground.hwnd)
        .filter(|hwnd| *hwnd != 0)
    {
        window::set_last_external_hwnd(hwnd);
    }
    tracing::debug!(
        foreground_app = ?snapshot.foreground_app.as_ref().map(|f| &f.process_name),
        window_title = ?snapshot.foreground_app.as_ref().map(|f| &f.window_title),
        "invoke: captured context"
    );

    // 2. 更新 SearchService 中的快照
    //
    // 选区抓取采用「快速捕获 + 慢速异步提取」模式：
    // - show() 之前：capture_focused_element() 仅做 GetFocusedElement()（O(1)，<5ms）
    // - show() 之后：spawn 线程做三段式 TextPattern 提取（可能 100-500ms）
    // 这样窗口显示不被 UIA 阻塞——慢应用上用户不再感到"卡一下才出来"。
    //
    // 提取完成后通过 update_selected_text 回填 + emit awareness-updated 触发前端 retrigger。
    let focused_element = if context_cfg.selection_enabled {
        let t_capture = std::time::Instant::now();
        let focused = snapshot
            .foreground_app
            .as_ref()
            .filter(|fg| fg.hwnd != 0)
            .and_then(|_| crate::infra::platform::selection::capture_focused_element());
        tracing::debug!(
            target: "perf",
            capture_ms = t_capture.elapsed().as_millis(),
            has_element = focused.is_some(),
            "[perf] invoke: capture_focused_element (before show)"
        );
        focused
    } else {
        None
    };

    if let Some(search_service) = app.try_state::<Arc<SearchService>>() {
        search_service.update_snapshot(snapshot.clone());
    }

    // 3. 调 infra primitive：定位 → show → set_focus → emit SHOWN
    if let Err(e) = window::show_main_window(app) {
        // show 失败已在 infra 层记 warn，这里不重复
        tracing::debug!(error = %e, "invoke: show_main_window 失败");
        return;
    }

    // 4. show 之后：异步提取选区（不阻塞窗口显示）
    //
    // focused_element 在 show() 之前通过 GetFocusedElement() 捕获，
    // 此时焦点还在原应用上。show() 之后焦点已移到 Blink，但捕获的 COM 元素
    // 仍然指向原应用的焦点控件——MTA 公寓下 COM 接口跨线程安全。
    //
    // 提取完成后回填 SearchService 快照 + emit awareness-updated 触发前端 retrigger，
    // 让翻译 Ghost 等依赖选区的建议在选区就绪后自动出现。
    if let Some(focused) = focused_element {
        let search_service = app
            .try_state::<Arc<SearchService>>()
            .map(|s| s.inner().clone());
        let app_clone = app.clone();
        std::thread::spawn(move || {
            let t_extract = std::time::Instant::now();
            let grabbed =
                crate::infra::platform::selection::extract_selection_from_element(&focused)
                    .or_else(|| {
                        // UIA 未命中，回退鼠标钩子缓存
                        let cached = crate::infra::platform::selection::get_last_selection();
                        if cached.is_some() {
                            tracing::trace!("invoke: 回退到鼠标钩子选区缓存");
                        }
                        cached.map(|(text, _)| text)
                    });
            let hit = grabbed.is_some();
            if let Some(ref text) = grabbed {
                tracing::debug!(len = text.chars().count(), "invoke: UIA 异步抓取选区成功");
            }
            if let Some(ss) = search_service {
                ss.update_selected_text(grabbed, None);
                // 通知前端重跑搜索——选区可能刚到，翻译 Ghost 等建议需要更新
                // 仅在窗口仍可见时 emit（用户可能已 ESC 关闭）
                if window::is_visible() {
                    let _ = app_clone.emit(EventNames::AWARENESS_UPDATED, ());
                }
            }
            tracing::debug!(
                target: "perf",
                extract_ms = t_extract.elapsed().as_millis(),
                hit,
                "[perf] invoke: async UIA extraction (after show)"
            );
        });
    }
}

/// 隐藏 chat 窗口编排：先中止 AI active request，再隐藏窗口。
///
/// 0.21.14：从 `infra/platform/window/windows.rs::hide_chat_window` 上移。
/// infra 层 `hide_chat_window_primitive` 只负责 hide 窗口，
/// 本函数负责定位 ChatService 并调 `abort_active`。
pub fn hide_chat_window(app: &AppHandle) {
    // 先 abort active request
    if let Some(cs) = app.try_state::<Arc<crate::domain::ai::chat_service::ChatService>>() {
        cs.abort_active();
    }
    // 再隐藏窗口
    window::hide_chat_window_primitive(app);
}

// ── 窗口事件回调类型（0.21.14）────────────────────────────────────────────
//
// 回调类型定义已下沉到 infra 层（`infra/platform/window/windows.rs`），
// 与 `StickyCloseFallback` 同模式：infra 定义容器类型 + try_state 消费，
// app 层注入闭包实现。这里仅重导出，供 main.rs wiring 使用。

pub use crate::infra::platform::window::{
    ChatCloseCallback, StickySpareCloseCallback, WelcomeCloseCallback,
};
