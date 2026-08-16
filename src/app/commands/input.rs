//! 输入 UI 状态 command。
//!
//! 前端通过 `register_main_input_view` 获取初始快照 + view_epoch，
//! 通过 `update_main_input_context` 上报离散视图上下文变化（query 是否为空 / AI mode / 剪贴板模式）。
//! 后端输入状态机据此决定 native exclusive chord session 的建立与退出。
//! 0.20.8: 新增 `clipboard_mode` 字段——独占模式之一，与 `ai_mode` 对称地抑制 chord 独占会话。

use serde::Serialize;

use crate::infra::platform::hotkey::{
    InputController, InputUiState, MainViewContext, alloc_view_epoch, get_latest_ui_state,
};

/// `register_main_input_view` 的返回值。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisterViewResult {
    pub view_epoch: u64,
    pub state: InputUiState,
}

/// 注册主窗口输入视图，返回 view_epoch + 当前 UI 状态快照。
///
/// 前端初始化时调用：
/// 1. 先注册 `INPUT_STATE_CHANGED` listener。
/// 2. 再调用此 command 获取初始快照 + view_epoch。
/// 3. 用 `state.revision` 与 listener 已收到的事件比较，只接受更新的一份。
///
/// 后端分配非 0 view_epoch，建立 `ready=true/revision=0` 的 view context，
/// 并发送到 hook 线程更新输入状态机。
///
/// # 参数
/// - `query_empty`: query 输入框 trim 后是否为空
/// - `ai_mode`: AI 模式是否活跃（独占模式之一）
/// - `clipboard_mode`: 剪贴板模式是否活跃（0.20.8 新增，独占模式之一）
#[tauri::command]
pub fn register_main_input_view(
    query_empty: bool,
    ai_mode: bool,
    clipboard_mode: bool,
) -> RegisterViewResult {
    let view_epoch = alloc_view_epoch();
    let ctx = MainViewContext {
        view_epoch,
        revision: 0,
        ready: true,
        query_empty,
        ai_mode,
        clipboard_mode,
    };
    InputController::update_view(ctx);
    let state = get_latest_ui_state();
    RegisterViewResult { view_epoch, state }
}

/// 更新主窗口输入视图上下文（query 是否为空 / AI mode / 剪贴板模式变化时调用）。
///
/// 前端只在 `query_empty`、`ai_mode` 或 `clipboard_mode` 实际发生变化时调用，不逐字符发送。
/// 携带 `view_epoch` + 递增 `revision`；旧 epoch 的 update 被后端丢弃。
///
/// # 参数
/// - `view_epoch`: register_main_input_view 返回的 epoch
/// - `revision`: 前端递增的 context revision
/// - `query_empty`: query 输入框 trim 后是否为空
/// - `ai_mode`: AI 模式是否活跃（独占模式之一）
/// - `clipboard_mode`: 剪贴板模式是否活跃（0.20.8 新增，独占模式之一）
#[tauri::command]
pub fn update_main_input_context(
    view_epoch: u64,
    revision: u64,
    query_empty: bool,
    ai_mode: bool,
    clipboard_mode: bool,
) {
    let ctx = MainViewContext {
        view_epoch,
        revision,
        ready: true,
        query_empty,
        ai_mode,
        clipboard_mode,
    };
    InputController::update_view(ctx);
}
