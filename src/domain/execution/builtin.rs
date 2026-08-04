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
        cx.env
            .show_sticky_manager()
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

// 0.14.4: OpenUrlAction / OpenPathAction / RevealInExplorerAction 已删除。
// 它们的功能由 Capability 版本承担（src/domain/capability/builtins/open_url.rs 等）。
// run_builtin_action 命令在 ActionRegistry 未命中时 fallback 到 CapabilityRegistry。
// 0.17.6: confirm_ai_action 已删除（主窗口 AI 改走 ChatService + confirm_chat_action）。
