//! 内置动作实现（0.8.6 §8.1.1 + §8.2.4；0.9.0 §3.3 tool-call 进化）。
//!
//! 12 个内置动作，每个一个 struct `impl Action`。
//! 从 `commands.rs::execute_builtin_action` 的 match 分支迁移而来。
//!
//! **i18n**（0.8.6 §8.2.4）：title/subtitle 走 `LocalizableText`，
//! `list_builtin_actions` 按当前 UI 语言 resolve。
//!
//! **0.9.0 §3.3 演进**：所有 12 个动作**显式覆盖** `schema()` + `danger_class()`——
//! 即使多数是 `Safe`，也强制开发者思考,防漏于千里之堤。参数化 3 个（`OpenUrl` /
//! `OpenPath` / `RevealInExplorer`）声明语义键（`url` / `path`）+ 保留 `_legacy_arg`
//! 兼容层直到 0.9.2 前端契约演进。

use crate::domain::plugin::LocalizableText;

use super::{Action, ActionContext, ActionOutcome, ActionSchema, DangerClass, ExecError};

/// zh/en 双语便捷构造。
fn bilingual(zh: &str, en: &str) -> LocalizableText {
    let mut map = std::collections::HashMap::new();
    map.insert("zh".to_string(), zh.to_string());
    map.insert("en".to_string(), en.to_string());
    LocalizableText::Localized(map)
}

/// 参数化动作:构造带单个 required string 字段的 JSON Schema。
///
/// 例:`param_string_schema("open_url", "打开链接", "url", "要打开的 URL")`
///  → `{ name, description, parameters: { type:"object", properties: { url: string }, required: ["url"] } }`
#[allow(dead_code)] // 0.9.0 由 3 个参数化 builtin 调用；trait method 层挂了 allow,这里同步挂
fn param_string_schema(
    name: &str,
    description: &str,
    param_key: &str,
    param_description: &str,
) -> ActionSchema {
    ActionSchema {
        name: name.to_string(),
        description: description.to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                param_key: { "type": "string", "description": param_description }
            },
            "required": [param_key]
        }),
    }
}

// ── 无参动作 ──────────────────────────────────────────────

pub struct OpenSettingsAction;
#[async_trait::async_trait]
impl Action for OpenSettingsAction {
    fn id(&self) -> &str {
        "open_settings"
    }
    fn title(&self) -> &LocalizableText {
        static T: std::sync::OnceLock<LocalizableText> = std::sync::OnceLock::new();
        T.get_or_init(|| bilingual("打开设置", "Open Settings"))
    }
    fn subtitle(&self) -> &LocalizableText {
        static T: std::sync::OnceLock<LocalizableText> = std::sync::OnceLock::new();
        T.get_or_init(|| bilingual("Blink 偏好设置", "Blink Preferences"))
    }
    fn schema(&self) -> ActionSchema {
        ActionSchema::empty("open_settings", "Open the Blink settings window")
    }
    fn danger_class(&self) -> DangerClass {
        DangerClass::Safe
    }
    async fn execute(&self, cx: &ActionContext<'_>) -> Result<ActionOutcome, ExecError> {
        tracing::debug!("执行内置动作：打开设置");
        cx.env.hide_main_window("open_settings");
        cx.env.open_settings();
        Ok(ActionOutcome::Nop)
    }
}

/// 便签管理（0.16.10）——打开便签管理窗口。
pub struct ShowStickyManagerAction;
#[async_trait::async_trait]
impl Action for ShowStickyManagerAction {
    fn id(&self) -> &str {
        "sticky_manager"
    }
    fn title(&self) -> &LocalizableText {
        static T: std::sync::OnceLock<LocalizableText> = std::sync::OnceLock::new();
        T.get_or_init(|| bilingual("便签管理", "Sticky Manager"))
    }
    fn subtitle(&self) -> &LocalizableText {
        static T: std::sync::OnceLock<LocalizableText> = std::sync::OnceLock::new();
        T.get_or_init(|| bilingual("管理桌面便签", "Manage desktop sticky notes"))
    }
    fn schema(&self) -> ActionSchema {
        ActionSchema::empty("sticky_manager", "Open the sticky notes manager window")
    }
    fn danger_class(&self) -> DangerClass {
        DangerClass::Safe
    }
    async fn execute(&self, cx: &ActionContext<'_>) -> Result<ActionOutcome, ExecError> {
        tracing::debug!("执行内置动作：便签管理");
        cx.env.hide_main_window("sticky_manager");
        cx.env.show_sticky_manager().map_err(ExecError::Runtime)?;
        Ok(ActionOutcome::Nop)
    }
}

/// 打开当前剪贴板图片进入用户侧通用编辑会话。
pub struct EditClipboardImageAction;
#[async_trait::async_trait]
impl Action for EditClipboardImageAction {
    fn id(&self) -> &str {
        "edit_clipboard_image"
    }
    fn title(&self) -> &LocalizableText {
        static T: std::sync::OnceLock<LocalizableText> = std::sync::OnceLock::new();
        T.get_or_init(|| bilingual("编辑剪贴板图片", "Edit Clipboard Image"))
    }
    fn subtitle(&self) -> &LocalizableText {
        static T: std::sync::OnceLock<LocalizableText> = std::sync::OnceLock::new();
        T.get_or_init(|| bilingual("标注当前剪贴板图片", "Annotate the current clipboard image"))
    }
    fn schema(&self) -> ActionSchema {
        ActionSchema::empty(
            "edit_clipboard_image",
            "Open the current clipboard image in the local annotation editor",
        )
    }
    fn danger_class(&self) -> DangerClass {
        DangerClass::Safe
    }
    async fn execute(&self, cx: &ActionContext<'_>) -> Result<ActionOutcome, ExecError> {
        tracing::debug!("执行内置动作：编辑剪贴板图片");
        let content = crate::domain::clipboard::read_current()
            .await
            .map_err(|error| ExecError::Runtime(error.to_string()))?;
        let crate::domain::clipboard::ClipboardContent::ImagePng(png_data) = content else {
            return Err(ExecError::Runtime("当前剪贴板中没有图片".into()));
        };
        cx.env
            .show_image_editor(png_data)
            .map_err(ExecError::Runtime)?;
        Ok(ActionOutcome::Nop)
    }
}

pub struct LockWorkstationAction;
#[async_trait::async_trait]
impl Action for LockWorkstationAction {
    fn id(&self) -> &str {
        "lock"
    }
    fn title(&self) -> &LocalizableText {
        static T: std::sync::OnceLock<LocalizableText> = std::sync::OnceLock::new();
        T.get_or_init(|| bilingual("锁定电脑", "Lock Workstation"))
    }
    fn subtitle(&self) -> &LocalizableText {
        static T: std::sync::OnceLock<LocalizableText> = std::sync::OnceLock::new();
        T.get_or_init(|| bilingual("Lock Workstation", "Lock Workstation"))
    }
    fn schema(&self) -> ActionSchema {
        ActionSchema::empty("lock", "Lock the Windows workstation")
    }
    /// 系统级动作,AI 意外锁屏体验极差——Dangerous 白名单
    fn danger_class(&self) -> DangerClass {
        DangerClass::Dangerous
    }
    async fn execute(&self, _cx: &ActionContext<'_>) -> Result<ActionOutcome, ExecError> {
        // 0.14.6 §2.3：Win32 LockWorkStation 迁至 infra/platform/lock.rs
        let _ = crate::infra::platform::lock::lock_workstation();
        Ok(ActionOutcome::Nop)
    }
}

pub struct ShutdownAction;
#[async_trait::async_trait]
impl Action for ShutdownAction {
    fn id(&self) -> &str {
        "shutdown"
    }
    fn title(&self) -> &LocalizableText {
        static T: std::sync::OnceLock<LocalizableText> = std::sync::OnceLock::new();
        T.get_or_init(|| bilingual("关机", "Shutdown"))
    }
    fn subtitle(&self) -> &LocalizableText {
        static T: std::sync::OnceLock<LocalizableText> = std::sync::OnceLock::new();
        T.get_or_init(|| bilingual("Shutdown", "Shutdown"))
    }
    fn schema(&self) -> ActionSchema {
        ActionSchema::empty("shutdown", "Shut down the computer immediately")
    }
    /// 关机不可逆丢数据——Dangerous 白名单
    fn danger_class(&self) -> DangerClass {
        DangerClass::Dangerous
    }
    async fn execute(&self, _cx: &ActionContext<'_>) -> Result<ActionOutcome, ExecError> {
        #[cfg(target_os = "windows")]
        {
            let _ = std::process::Command::new("shutdown.exe")
                .args(["/s", "/t", "0"])
                .spawn();
        }
        Ok(ActionOutcome::Nop)
    }
}

pub struct RestartAction;
#[async_trait::async_trait]
impl Action for RestartAction {
    fn id(&self) -> &str {
        "restart"
    }
    fn title(&self) -> &LocalizableText {
        static T: std::sync::OnceLock<LocalizableText> = std::sync::OnceLock::new();
        T.get_or_init(|| bilingual("重启", "Restart"))
    }
    fn subtitle(&self) -> &LocalizableText {
        static T: std::sync::OnceLock<LocalizableText> = std::sync::OnceLock::new();
        T.get_or_init(|| bilingual("Restart", "Restart"))
    }
    fn schema(&self) -> ActionSchema {
        ActionSchema::empty("restart", "Restart the computer immediately")
    }
    /// 重启会丢未保存数据——Dangerous 白名单
    fn danger_class(&self) -> DangerClass {
        DangerClass::Dangerous
    }
    async fn execute(&self, _cx: &ActionContext<'_>) -> Result<ActionOutcome, ExecError> {
        #[cfg(target_os = "windows")]
        {
            let _ = std::process::Command::new("shutdown.exe")
                .args(["/r", "/t", "0"])
                .spawn();
        }
        Ok(ActionOutcome::Nop)
    }
}

pub struct SleepAction;
#[async_trait::async_trait]
impl Action for SleepAction {
    fn id(&self) -> &str {
        "sleep"
    }
    fn title(&self) -> &LocalizableText {
        static T: std::sync::OnceLock<LocalizableText> = std::sync::OnceLock::new();
        T.get_or_init(|| bilingual("睡眠", "Sleep"))
    }
    fn subtitle(&self) -> &LocalizableText {
        static T: std::sync::OnceLock<LocalizableText> = std::sync::OnceLock::new();
        T.get_or_init(|| bilingual("Sleep", "Sleep"))
    }
    fn schema(&self) -> ActionSchema {
        ActionSchema::empty("sleep", "Put the computer to sleep")
    }
    /// AI 自动进睡眠打断用户工作流——Dangerous 白名单
    fn danger_class(&self) -> DangerClass {
        DangerClass::Dangerous
    }
    async fn execute(&self, _cx: &ActionContext<'_>) -> Result<ActionOutcome, ExecError> {
        #[cfg(target_os = "windows")]
        {
            let _ = std::process::Command::new("rundll32.exe")
                .args(["powrprof.dll,SetSuspendState", "0,1,0"])
                .spawn();
        }
        Ok(ActionOutcome::Nop)
    }
}

pub struct ClearHistoryAction;
#[async_trait::async_trait]
impl Action for ClearHistoryAction {
    fn id(&self) -> &str {
        "clear_history"
    }
    fn title(&self) -> &LocalizableText {
        static T: std::sync::OnceLock<LocalizableText> = std::sync::OnceLock::new();
        T.get_or_init(|| bilingual("清空搜索历史", "Clear Search History"))
    }
    fn subtitle(&self) -> &LocalizableText {
        static T: std::sync::OnceLock<LocalizableText> = std::sync::OnceLock::new();
        T.get_or_init(|| bilingual("清除所有应用启动记录", "Clear all app launch records"))
    }
    fn schema(&self) -> ActionSchema {
        ActionSchema::empty(
            "clear_history",
            "Clear all app launch history records (irreversible)",
        )
    }
    /// 历史数据不可逆——Dangerous 白名单
    fn danger_class(&self) -> DangerClass {
        DangerClass::Dangerous
    }
    async fn execute(&self, cx: &ActionContext<'_>) -> Result<ActionOutcome, ExecError> {
        let pool = &cx.env.db_pools().history;
        crate::infra::data::history::clear(&pool).await;
        tracing::info!("搜索历史已清空");
        Ok(ActionOutcome::Nop)
    }
}

pub struct ExitBlinkAction;
#[async_trait::async_trait]
impl Action for ExitBlinkAction {
    fn id(&self) -> &str {
        "exit_blink"
    }
    fn title(&self) -> &LocalizableText {
        static T: std::sync::OnceLock<LocalizableText> = std::sync::OnceLock::new();
        T.get_or_init(|| bilingual("退出 Blink", "Exit Blink"))
    }
    fn subtitle(&self) -> &LocalizableText {
        static T: std::sync::OnceLock<LocalizableText> = std::sync::OnceLock::new();
        T.get_or_init(|| bilingual("Exit Blink Launcher", "Exit Blink Launcher"))
    }
    fn schema(&self) -> ActionSchema {
        ActionSchema::empty("exit_blink", "Exit the Blink launcher process")
    }
    /// 退出主体让用户失去 Blink——Dangerous 白名单
    fn danger_class(&self) -> DangerClass {
        DangerClass::Dangerous
    }
    async fn execute(&self, cx: &ActionContext<'_>) -> Result<ActionOutcome, ExecError> {
        cx.env.exit_app();
        Ok(ActionOutcome::Nop)
    }
}

pub struct OpenLogsAction;
#[async_trait::async_trait]
impl Action for OpenLogsAction {
    fn id(&self) -> &str {
        "open_logs"
    }
    fn title(&self) -> &LocalizableText {
        static T: std::sync::OnceLock<LocalizableText> = std::sync::OnceLock::new();
        T.get_or_init(|| bilingual("打开日志文件", "Open Log File"))
    }
    fn subtitle(&self) -> &LocalizableText {
        static T: std::sync::OnceLock<LocalizableText> = std::sync::OnceLock::new();
        T.get_or_init(|| bilingual("Open Blink Log File", "Open Blink Log File"))
    }
    fn schema(&self) -> ActionSchema {
        ActionSchema::empty(
            "open_logs",
            "Open the Blink log file with the default viewer",
        )
    }
    fn danger_class(&self) -> DangerClass {
        DangerClass::Safe
    }
    async fn execute(&self, _cx: &ActionContext<'_>) -> Result<ActionOutcome, ExecError> {
        tracing::debug!("执行内置动作：打开日志文件");
        let log_path = crate::infra::utils::logging::current_log_file();
        let log_dir = crate::infra::utils::logging::log_dir();
        tracing::debug!(log_path = %log_path.display(), log_dir = %log_dir.display(), "日志路径");

        if log_path.exists() {
            tracing::debug!(path = %log_path.display(), "日志文件存在，打开");
            if let Err(e) = open::that(&log_path) {
                tracing::error!(error = %e, "打开日志文件失败，尝试打开目录");
                let _ = open::that(&log_dir);
            }
        } else {
            tracing::debug!("日志文件不存在，打开目录");
            let _ = open::that(&log_dir);
        }
        Ok(ActionOutcome::Nop)
    }
}

pub struct OpenDataDirAction;
#[async_trait::async_trait]
impl Action for OpenDataDirAction {
    fn id(&self) -> &str {
        "open_data_dir"
    }
    fn title(&self) -> &LocalizableText {
        static T: std::sync::OnceLock<LocalizableText> = std::sync::OnceLock::new();
        T.get_or_init(|| bilingual("打开数据目录", "Open Data Directory"))
    }
    fn subtitle(&self) -> &LocalizableText {
        static T: std::sync::OnceLock<LocalizableText> = std::sync::OnceLock::new();
        T.get_or_init(|| bilingual("Open Blink Data Folder", "Open Blink Data Folder"))
    }
    fn schema(&self) -> ActionSchema {
        ActionSchema::empty(
            "open_data_dir",
            "Open the Blink data folder in the system file manager",
        )
    }
    fn danger_class(&self) -> DangerClass {
        DangerClass::Safe
    }
    async fn execute(&self, _cx: &ActionContext<'_>) -> Result<ActionOutcome, ExecError> {
        tracing::debug!("执行内置动作：打开数据目录");
        let dir = crate::infra::utils::paths::app_data_dir();
        tracing::debug!(dir = %dir.display(), "数据目录路径");
        if let Err(e) = open::that(&dir) {
            tracing::error!(error = %e, dir = %dir.display(), "打开数据目录失败");
        }
        Ok(ActionOutcome::Nop)
    }
}

/// Blink 通用调试信息（0.19.17 首版重点覆盖 Windows 输入链路）。
///
/// 采集 InputState 快照、Hook 状态、物理修饰键、已发布 UI 状态和最近事件环形缓冲区，
/// 格式化为可读文本并复制到剪贴板。后续版本可在同一动作中扩展其他运行时模块。
pub struct BlinkPrintDebugInfoAction;

#[async_trait::async_trait]
impl Action for BlinkPrintDebugInfoAction {
    fn id(&self) -> &str {
        "blink_print_debug_info"
    }
    fn title(&self) -> &LocalizableText {
        static T: std::sync::OnceLock<LocalizableText> = std::sync::OnceLock::new();
        T.get_or_init(|| bilingual("Blink Print Debug Info", "Blink Print Debug Info"))
    }
    fn subtitle(&self) -> &LocalizableText {
        static T: std::sync::OnceLock<LocalizableText> = std::sync::OnceLock::new();
        T.get_or_init(|| {
            bilingual(
                "复制 Blink 通用调试信息；当前版本详细包含 Windows Hook 与输入状态",
                "Copy general Blink debug info, currently with detailed Windows hook and input state",
            )
        })
    }
    fn schema(&self) -> ActionSchema {
        ActionSchema::empty(
            "blink_print_debug_info",
            "Collect general Blink runtime debug info and copy it to the clipboard; this version includes detailed Windows input diagnostics",
        )
    }
    fn danger_class(&self) -> DangerClass {
        DangerClass::Safe
    }
    async fn execute(&self, _cx: &ActionContext<'_>) -> Result<ActionOutcome, ExecError> {
        tracing::info!("执行内置动作：Blink Print Debug Info");

        let physical = crate::infra::platform::hotkey::read_physical_modifiers();
        let snapshot =
            crate::infra::platform::hotkey::diagnostics::take_diagnostic_snapshot(physical);
        let events = crate::infra::platform::hotkey::diagnostics::take_diagnostic_events();
        let text = format_diagnostic_info(&snapshot, &events);

        // 复制到剪贴板（skip_persist = true，不写入剪贴板历史）
        if let Err(e) = crate::infra::platform::clipboard::write_text_to_clipboard(
            &text,
            "blink_print_debug_info",
            true,
        ) {
            tracing::error!(error = %e, "写入诊断信息到剪贴板失败");
        }

        Ok(ActionOutcome::Nop)
    }
}

/// Blink 调试快照 + Windows Hook 恢复（0.19.17）。
///
/// 与 `BlinkPrintDebugInfoAction` 相同地采集诊断快照并复制到剪贴板，
/// **额外**触发一次 `ManualRecovery` 请求——当用户怀疑 Hook 已失效时，
/// 执行此动作可在采集诊断的同时尝试恢复 Hook。
pub struct BlinkDebugInitHookAction;

#[async_trait::async_trait]
impl Action for BlinkDebugInitHookAction {
    fn id(&self) -> &str {
        "blink_debug_inithook"
    }
    fn title(&self) -> &LocalizableText {
        static T: std::sync::OnceLock<LocalizableText> = std::sync::OnceLock::new();
        T.get_or_init(|| bilingual("Blink Debug InitHook", "Blink Debug InitHook"))
    }
    fn subtitle(&self) -> &LocalizableText {
        static T: std::sync::OnceLock<LocalizableText> = std::sync::OnceLock::new();
        T.get_or_init(|| {
            bilingual(
                "打印调试信息，并在安全门禁满足后重置输入状态与重装 Windows Hook",
                "Print debug info, then safely reset input state and reinstall the Windows hook",
            )
        })
    }
    fn schema(&self) -> ActionSchema {
        ActionSchema::empty(
            "blink_debug_inithook",
            "Collect current Blink debug info, then safely reset volatile Windows input state and request hook reinstallation",
        )
    }
    fn danger_class(&self) -> DangerClass {
        DangerClass::Safe
    }
    async fn execute(&self, _cx: &ActionContext<'_>) -> Result<ActionOutcome, ExecError> {
        tracing::info!("执行内置动作：恢复输入钩子（诊断 + ManualRecovery）");

        // 1. 采集诊断快照（与 BlinkPrintDebugInfoAction 相同）
        let physical = crate::infra::platform::hotkey::read_physical_modifiers();
        let snapshot =
            crate::infra::platform::hotkey::diagnostics::take_diagnostic_snapshot(physical);
        let events = crate::infra::platform::hotkey::diagnostics::take_diagnostic_events();
        let text = format_diagnostic_info(&snapshot, &events);

        if let Err(e) = crate::infra::platform::clipboard::write_text_to_clipboard(
            &text,
            "blink_debug_inithook",
            true,
        ) {
            tracing::error!(error = %e, "写入诊断信息到剪贴板失败");
        }

        // 2. 请求手动 Hook 恢复
        crate::infra::platform::hotkey::InputController::request_manual_recovery();
        tracing::info!("ManualRecovery 已请求");

        Ok(ActionOutcome::Nop)
    }
}

/// 格式化诊断快照为可读文本。
fn format_diagnostic_info(
    snapshot: &crate::infra::platform::hotkey::diagnostics::InputDiagnosticSnapshot,
    events: &[crate::infra::platform::hotkey::diagnostics::InputDiagnosticEvent],
) -> String {
    let mut lines = Vec::new();

    lines.push("=== Blink Debug Info ===".to_string());
    lines.push("Schema: 1".to_string());
    lines.push("Profile: windows_input".to_string());
    lines.push(format!("Version: {}", env!("CARGO_PKG_VERSION")));
    lines.push(format!(
        "Platform: {} / {}",
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    lines.push(format!("Uptime: {}ms", snapshot.uptime_ms));
    lines.push(String::new());

    // ── Modifiers ──
    lines.push("--- Modifiers (Level | Physical) ---".to_string());
    let key_names = [
        "LCtrl", "RCtrl", "LShift", "RShift", "LAlt", "RAlt", "LMeta", "RMeta",
    ];
    let phys = [
        snapshot.physical.lctrl,
        snapshot.physical.rctrl,
        snapshot.physical.lshift,
        snapshot.physical.rshift,
        snapshot.physical.lalt,
        snapshot.physical.ralt,
        snapshot.physical.lmeta,
        snapshot.physical.rmeta,
    ];
    for (i, name) in key_names.iter().enumerate() {
        let level = level_str(snapshot.state.modifier_levels[i]);
        let p = if phys[i] { "Down" } else { "Up" };
        lines.push(format!("{name:>6}: {level:>12} | {p}"));
    }
    lines.push(format!(
        "Pressed mask: 0x{:04x}",
        snapshot.state.pressed_mask
    ));
    lines.push(String::new());

    // ── Gesture ──
    lines.push("--- Gesture ---".to_string());
    let gesture = if snapshot.state.gesture_idle {
        "Idle"
    } else if snapshot.state.gesture_armed {
        "Armed"
    } else {
        "Active"
    };
    lines.push(format!("State: {gesture}"));
    lines.push(String::new());

    // ── Chord ──
    lines.push("--- Chord ---".to_string());
    lines.push(format!(
        "Active: {}, Session: {:?}",
        snapshot.state.chord_active, snapshot.state.chord_session_id
    ));
    lines.push(String::new());

    // ── Voice / Recorder ──
    lines.push("--- Voice / Recorder ---".to_string());
    lines.push(format!(
        "Voice: {}, Recorder: {}",
        if snapshot.state.voice_idle {
            "Idle"
        } else {
            "Active"
        },
        if snapshot.state.recorder_idle {
            "Idle"
        } else {
            "Active"
        },
    ));
    lines.push(String::new());

    // ── Window ──
    lines.push("--- Window ---".to_string());
    lines.push(format!(
        "Visible: {}, Revision: {}",
        snapshot.state.window_visible, snapshot.state.window_revision
    ));
    lines.push(String::new());

    // ── View ──
    lines.push("--- View ---".to_string());
    lines.push(format!(
        "Ready: {}, Epoch: {}, Revision: {}",
        snapshot.state.view_ready, snapshot.state.view_epoch, snapshot.state.view_revision
    ));
    lines.push(format!(
        "QueryEmpty: {}, AiMode: {}",
        snapshot.state.view_query_empty, snapshot.state.view_ai_mode
    ));
    lines.push(String::new());

    // ── Config ──
    lines.push("--- Config ---".to_string());
    lines.push(format!("Revision: {}", snapshot.state.config_revision));
    lines.push(String::new());

    // ── UI State ──
    lines.push("--- UI State ---".to_string());
    lines.push(format!(
        "Desired:  Alt={} Chord={} Rev={}",
        snapshot.state.desired_alt_down,
        snapshot.state.desired_chord_active,
        snapshot.state.desired_revision
    ));
    lines.push(format!(
        "Published: Alt={} Chord={} Rev={}",
        snapshot.published_alt_down, snapshot.published_chord_active, snapshot.published_revision
    ));
    lines.push(String::new());

    // ── Hook ──
    lines.push("--- Hook ---".to_string());
    lines.push(format!(
        "Installed: {}, Available: {}",
        snapshot.hook.hook_installed, snapshot.hook.hook_available
    ));
    lines.push(format!(
        "PendingReinstall: {:?}, Attempt: {}",
        snapshot.hook.pending_reinstall, snapshot.hook.reinstall_attempt
    ));
    lines.push(format!(
        "WTS: {}, Raw: {}",
        snapshot.hook.wts_registered, snapshot.hook.raw_registered
    ));
    lines.push(String::new());

    // ── Recent Events ──
    lines.push(format!("--- Recent Events ({}) ---", events.len()));
    for event in events.iter().rev().take(20) {
        lines.push(format_event(event));
    }
    lines.push(String::new());

    // ── Findings ──
    lines.push("--- Findings ---".to_string());
    let mut findings = Vec::new();
    for (i, name) in key_names.iter().enumerate() {
        let cached_down = snapshot.state.modifier_levels[i].is_pressed();
        if cached_down != phys[i] {
            findings.push(format!(
                "ERROR MODIFIER_MISMATCH: {name} cached={} physical={}",
                if cached_down { "Down" } else { "Up" },
                if phys[i] { "Down" } else { "Up" }
            ));
        }
    }
    if snapshot.state.desired_revision != snapshot.published_revision
        || snapshot.state.desired_alt_down != snapshot.published_alt_down
        || snapshot.state.desired_chord_active != snapshot.published_chord_active
    {
        findings.push(format!(
            "ERROR UI_PROJECTION_MISMATCH: desired_rev={} published_rev={}",
            snapshot.state.desired_revision, snapshot.published_revision
        ));
    }
    if !snapshot.hook.hook_installed || !snapshot.hook.hook_available {
        findings.push("ERROR HOOK_UNAVAILABLE".to_string());
    }
    if findings.is_empty() {
        lines.push("OK: no known input inconsistency detected".to_string());
    } else {
        lines.extend(findings);
    }

    lines.join("\n")
}

/// 格式化 ModifierLevel 为短字符串。
fn level_str(level: crate::infra::platform::hotkey::ModifierLevel) -> &'static str {
    use crate::infra::platform::hotkey::ModifierLevel;
    match level {
        ModifierLevel::Unknown => "Unknown",
        ModifierLevel::Up => "Up",
        ModifierLevel::Down => "Down",
        ModifierLevel::InjectedDown => "InjectedDn",
        ModifierLevel::InferredDown => "InferredDn",
    }
}

/// 格式化单条诊断事件。
fn format_event(
    event: &crate::infra::platform::hotkey::diagnostics::InputDiagnosticEvent,
) -> String {
    use crate::infra::platform::hotkey::diagnostics::{
        DiagnosticKeyClass, DiagnosticSource, DiagnosticTransition,
    };

    let src = match event.source {
        DiagnosticSource::Hook => "Hook",
        DiagnosticSource::Raw => "Raw",
        DiagnosticSource::Physical => "Phys",
        DiagnosticSource::Control => "Ctrl",
        DiagnosticSource::SessionReset => "SReset",
        DiagnosticSource::HoldTimer => "Timer",
    };
    let key = match event.key {
        DiagnosticKeyClass::Modifier(m) => match m {
            crate::infra::platform::hotkey::ModifierKey::LCtrl => "LCtrl",
            crate::infra::platform::hotkey::ModifierKey::RCtrl => "RCtrl",
            crate::infra::platform::hotkey::ModifierKey::LShift => "LShift",
            crate::infra::platform::hotkey::ModifierKey::RShift => "RShift",
            crate::infra::platform::hotkey::ModifierKey::LAlt => "LAlt",
            crate::infra::platform::hotkey::ModifierKey::RAlt => "RAlt",
            crate::infra::platform::hotkey::ModifierKey::LMeta => "LMeta",
            crate::infra::platform::hotkey::ModifierKey::RMeta => "RMeta",
        },
        DiagnosticKeyClass::MainKey => "MainKey",
        DiagnosticKeyClass::OtherKey => "OtherKey",
        DiagnosticKeyClass::None => "-",
    };
    let trans = match event.transition {
        DiagnosticTransition::Down => "Down",
        DiagnosticTransition::Up => "Up",
        DiagnosticTransition::Reconcile => "Reconcile",
        DiagnosticTransition::ConfigChanged => "ConfigChg",
        DiagnosticTransition::WindowChanged => "WindowChg",
        DiagnosticTransition::VoicePhaseChanged => "VoiceChg",
        DiagnosticTransition::RecorderModeChanged => "RecorderChg",
        DiagnosticTransition::SessionReset => "SessionReset",
        DiagnosticTransition::ManualRecovery => "ManualRecovery",
        DiagnosticTransition::HoldDeadline => "HoldDeadline",
        DiagnosticTransition::RawDeviceRemoved => "DevRemoved",
    };
    let inj = match event.injected {
        Some(true) => " inj=T",
        Some(false) => " inj=F",
        None => "",
    };
    let chord = format!("{}→{}", event.chord_before, event.chord_after);
    let level = match (event.before_level, event.after_level) {
        (Some(before), Some(after)) => {
            format!(" level:{}→{}", level_str(before), level_str(after))
        }
        _ => String::new(),
    };

    format!(
        "[{:04}] +{}ms {}/{} {}{}{} chord:{}",
        event.seq, event.elapsed_ms, src, key, trans, inj, level, chord
    )
}

// 0.14.4: OpenUrlAction / OpenPathAction / RevealInExplorerAction 已删除。
// 它们的功能由 Capability 版本承担（src/domain/capability/builtins/open_url.rs 等）。
// run_builtin_action 命令在 ActionRegistry 未命中时 fallback 到 CapabilityRegistry。
// 0.17.6: confirm_ai_action 已删除（主窗口 AI 改走 ChatService + confirm_chat_action）。
