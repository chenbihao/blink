//! 窗口编排层（0.21.14）——从 `infra/platform/window/windows.rs` 上移的业务编排。
//!
//! **职责**：Tauri managed-state 定位领域服务 + 采集上下文 + 调 infra primitive。
//! `windows.rs` 只保留参数化 Win32 / Tauri 窗口 primitive，不反向依赖 domain。
//!
//! **关键函数**：
//! - `invoke()`：热键唤起编排（采集快照 → 更新 SearchService → show_main_window → 异步选区/文件选中）
//! - `hide_chat_window()`：隐藏 chat 窗口编排（abort_active → hide_chat_window_primitive）
//!
//! **0.27.3 代次保护**：show 后的异步回写（UIA 选区 / Shell 文件选中）携带
//! `update_snapshot` 后的快照代次，回写前经 `*_if_fresh` 核对——唤起 A→关闭→B
//! 场景旧结果不覆盖新快照。

use std::sync::Arc;

use tauri::{AppHandle, Emitter, Manager};

use crate::domain::config::ContextConfig;
use crate::domain::event_names::EventNames;
use crate::domain::search::SearchService;
use crate::infra::platform::context::CollectParams;
use crate::infra::platform::window;

/// 文件选中采集 worker 占位（§3.2 有界并发：同一时刻至多一个 COM 采集线程）。
static FILE_SELECTION_ACTIVE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// 唤起编排：采集上下文快照 → 更新 SearchService → show 主窗口 → 异步提取选区。
///
/// **采集时机很重要**：必须在 show() 之前调用，否则拿到的前台是 Blink 自己。
///
/// 0.21.14：从 `infra/platform/window/windows.rs::invoke` 上移。
/// infra 层 `show_main_window` 只负责定位 + show + focus + emit SHOWN，
/// 本函数负责 domain state 定位 + context 采集 + SearchService 更新 + 异步选区回填。
pub fn invoke(app: &AppHandle) {
    invoke_with_placement(app, window::MainWindowPlacement::Launcher);
}

/// 显式指定唤起用途，避免把定位信息缺失误判为普通 launcher。
pub fn invoke_with_placement(app: &AppHandle, placement: window::MainWindowPlacement) {
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
    let mut snapshot = crate::infra::platform::context::collect(&collect_params);
    // 0.26.20：更新提醒盖章——managed cell 零 IO 读取；「可用更新」三条件
    //（自动检查开 && 检测到新版 && 未 dismiss）由 update_reminder 判定，
    // UpdateProducer 只读快照（domain 不发网络请求）。
    snapshot.available_update = crate::app::update_reminder::snapshot_update_hint(app);
    // 0.26.16：粘贴锚点。正常路径取快照前台；快照为空有两种情形——
    // ① 前台是 Blink 自己的窗口（collect 主动跳过，见 context::windows
    //   「前台是 Blink 自己，跳过」）：此刻主窗尚未 show（不可见），前台 Blink
    //   窗口必非 main 自己（对话窗/便签等）——直接记录为锚点，否则残留旧值，
    //   「贴入本体窗口」会贴错目标（0.26.16 实机教训）；
    // ② 主窗已可见时的重复唤起：前台就是 main，不能记录（保持旧锚点）。
    if let Some(hwnd) = snapshot
        .foreground_app
        .as_ref()
        .map(|foreground| foreground.hwnd)
        .filter(|hwnd| *hwnd != 0)
    {
        window::set_last_external_hwnd(hwnd);
    } else if !window::is_visible() {
        if let Some(hwnd) = window::get_foreground_hwnd().filter(|hwnd| *hwnd != 0) {
            tracing::debug!(hwnd, "invoke: 前台是 Blink 自己的窗口，记录为粘贴锚点");
            window::set_last_external_hwnd(hwnd);
        }
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
    // 提取完成后通过 update_selected_text_if_fresh 回填（带代次核验，0.27.3）
    // + emit awareness-updated 触发前端 retrigger。
    let focused_element = if context_cfg.selection_enabled
        && matches!(placement, window::MainWindowPlacement::Launcher)
    {
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

    // 0.27.3：文件选中冻结（show 前）。前台是 Explorer（窗口/桌面同宿主 explorer.exe）
    // 且总开关与子开关都开时，记录冻结 HWND；Shell COM 采集在 show 后的 STA 线程执行。
    // 敏感门控已由 collect() 完成过（敏感前台 → 快照空 → foreground_app None）。
    let file_selection_hwnd = if context_cfg.enabled
        && context_cfg.file_selection_enabled
        && matches!(placement, window::MainWindowPlacement::Launcher)
    {
        snapshot
            .foreground_app
            .as_ref()
            .filter(|fg| {
                fg.hwnd != 0 && fg.process_name.eq_ignore_ascii_case("explorer.exe")
            })
            .map(|fg| fg.hwnd)
    } else {
        None
    };

    let search_service = app.try_state::<Arc<SearchService>>().map(|s| s.inner().clone());
    let snapshot_generation = if let Some(ss) = &search_service {
        ss.update_snapshot(snapshot.clone());
        Some(ss.current_snapshot_generation())
    } else {
        None
    };

    // 3. 调 infra primitive：定位 → show → set_focus → emit SHOWN
    if let Err(e) = window::show_main_window_with_placement(app, placement) {
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
    // 0.27.3：回写走代次核验——期间发生新 invoke 则本轮结果整体丢弃。
    if let Some(focused) = focused_element {
        let search_service = search_service.clone();
        let generation = snapshot_generation;
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
            if let (Some(ss), Some(snapshot_gen)) = (&search_service, generation) {
                ss.update_selected_text_if_fresh(snapshot_gen, grabbed, None);
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

    // 5. show 之后：异步采集 Explorer 文件选中（0.27.3，STA COM worker）
    //
    // 有界并发（同一时刻至多一个采集线程，占位失败本轮放弃——下轮唤起重试）；
    // 回写前四重复核：代次（SearchService 侧）、开关（重读 config）、来源仍属
    // explorer、敏感门控（explorer 本身被列敏感时放弃）。日志不打印完整路径。
    if let (Some(hwnd), Some(ss), Some(snapshot_gen)) = (file_selection_hwnd, &search_service, snapshot_generation) {
        if FILE_SELECTION_ACTIVE
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            tracing::trace!("invoke: 文件选中采集 worker 占用中，本轮跳过");
        } else {
            let ss = ss.clone();
            let cfg_cell = app
                .try_state::<Arc<std::sync::RwLock<ContextConfig>>>()
                .map(|c| c.inner().clone());
            let sensitive = context_cfg.sensitive_apps.clone();
            let app_clone = app.clone();
            std::thread::spawn(move || {
                let t_collect = std::time::Instant::now();
                let pid = crate::infra::platform::selection::files::window_pid(hwnd);
                let state = crate::infra::platform::selection::files::collect_selection(
                    hwnd, pid, snapshot_gen,
                );
                // 回写前复核：开关未翻转、来源窗口仍属 explorer、explorer 未被列敏感
                let cfg_now = cfg_cell
                    .as_ref()
                    .map(|c| c.read().unwrap().clone())
                    .unwrap_or_default();
                let gate_ok = cfg_now.enabled
                    && cfg_now.file_selection_enabled
                    && !sensitive
                        .iter()
                        .any(|name| name.trim().eq_ignore_ascii_case("explorer.exe"))
                    && crate::infra::platform::selection::files::window_belongs_to_explorer(hwnd);
                let hit = state.is_some();
                match state {
                    Some(state) if gate_ok => {
                        tracing::debug!(
                            count = state.items.len(),
                            truncated = state.truncated,
                            source_pid = state.source_pid,
                            "invoke: 文件选中采集成功"
                        );
                        ss.update_file_selection_if_fresh(snapshot_gen, Some(state));
                        if window::is_visible() {
                            let _ = app_clone.emit(EventNames::AWARENESS_UPDATED, ());
                        }
                    }
                    Some(_) => {
                        tracing::debug!("invoke: 文件选中采集完成但门控复核未过，丢弃");
                    }
                    None => {}
                }
                tracing::debug!(
                    target: "perf",
                    elapsed_ms = t_collect.elapsed().as_millis(),
                    hit,
                    "[perf] invoke: async file selection (after show)"
                );
                FILE_SELECTION_ACTIVE.store(false, std::sync::atomic::Ordering::SeqCst);
            });
        }
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
