//! Tauri command 层：前端 invoke 入口，组合 core/search/history 能力。
//!
//! 命令保持轻量——编排逻辑，不含业务实现。

use tauri::Manager;

/// 主窗口 ESC 调用：隐藏主窗口。
#[tauri::command]
pub fn hide_window(app: tauri::AppHandle) {
    crate::window::hide(&app, "ESC");
}

/// 隐藏设置窗口（供设置页的 ESC 调用）。
#[tauri::command]
pub fn hide_settings_window(app: tauri::AppHandle) {
    if let Some(win) = app.get_webview_window("settings") {
        let _ = win.hide();
        tracing::debug!("hide_settings_window: 隐藏设置窗口");
    }
}

/// 前端输入时调用:经 SearchService 多路召回(sync lane 同步返回首批)。
///
/// calc / 应用搜索 / 历史融合等逻辑已下沉到各 SearchEngine + SearchService(见 0.2 设计 §2)。
/// `seq` 为前端递增请求序号,async 增量结果(blink://results)回带同一 seq 供前端校验。
#[tauri::command]
pub async fn search_apps(
    query: String,
    seq: u64,
    app: tauri::AppHandle,
) -> Vec<crate::search::AppEntry> {
    tracing::debug!(%query, seq, "search_apps: 收到搜索请求");
    let service = app.state::<std::sync::Arc<crate::search::SearchService>>();
    let results = service.search(&query, seq).await;
    tracing::debug!(count = results.len(), %query, "search_apps: 返回结果");
    for (i, item) in results.iter().enumerate() {
        tracing::debug!(
            index = i,
            name = %item.name,
            lnk_path = %item.lnk_path,
            "搜索结果项"
        );
    }
    results
}

/// 前端回车/点击时调用：启动选中的应用或执行内置动作。
/// 计算结果无 lnk_path，忽略。
#[tauri::command]
pub async fn launch_app(app: tauri::AppHandle, lnk_path: String) -> Result<(), String> {
    if lnk_path.is_empty() {
        return Ok(());
    }

    tracing::debug!(%lnk_path, "launch_app: 收到打开请求");

    // 检查是否为内置动作
    if let Some(action) = parse_builtin_action(&lnk_path) {
        tracing::debug!(?action, "launch_app: 识别为内置动作");
        execute_builtin_action(&app, action).await?;
        // 所有内置动作都隐藏主窗口，设置窗口单独显示
        crate::window::hide(&app, "builtin action");
        return Ok(());
    }

    tracing::debug!(%lnk_path, "launch_app: 普通应用启动");

    // 普通应用启动
    let pool = app.state::<sqlx::SqlitePool>();
    crate::history::record_launch(&pool, &lnk_path).await;
    crate::search::launch(&lnk_path)?;
    crate::window::hide(&app, "launch");
    Ok(())
}

/// 解析内置动作标识。
fn parse_builtin_action(path: &str) -> Option<BuiltinActionKind> {
    match path {
        "__BLINK_ACTION_OPEN_SETTINGS__" => Some(BuiltinActionKind::OpenSettings),
        "__BLINK_ACTION_LOCK__" => Some(BuiltinActionKind::LockWorkstation),
        "__BLINK_ACTION_SHUTDOWN__" => Some(BuiltinActionKind::Shutdown),
        "__BLINK_ACTION_RESTART__" => Some(BuiltinActionKind::Restart),
        "__BLINK_ACTION_SLEEP__" => Some(BuiltinActionKind::Sleep),
        "__BLINK_ACTION_CLEAR_HISTORY__" => Some(BuiltinActionKind::ClearHistory),
        "__BLINK_ACTION_EXIT__" => Some(BuiltinActionKind::ExitBlink),
        "__BLINK_ACTION_OPEN_LOGS__" => Some(BuiltinActionKind::OpenLogs),
        "__BLINK_ACTION_OPEN_DATA_DIR__" => Some(BuiltinActionKind::OpenDataDir),
        _ => None,
    }
}

/// 内置动作类型。
#[derive(Debug, Clone, Copy)]
enum BuiltinActionKind {
    OpenSettings,
    LockWorkstation,
    Shutdown,
    Restart,
    Sleep,
    ClearHistory,
    ExitBlink,
    OpenLogs,
    OpenDataDir,
}

/// 执行内置动作。
async fn execute_builtin_action(app: &tauri::AppHandle, action: BuiltinActionKind) -> Result<(), String> {
    match action {
        BuiltinActionKind::OpenSettings => {
            // 打开设置窗口（已存在则聚焦，否则创建）
            tracing::debug!("执行内置动作：打开设置");
            if let Some(w) = app.get_webview_window("settings") {
                let _ = w.show();
                let _ = w.set_focus();
            } else {
                use tauri::WebviewWindowBuilder;
                use tauri::WebviewUrl;
                tracing::debug!("设置窗口不存在，创建新窗口");
                let _ = WebviewWindowBuilder::new(
                    app,
                    "settings",
                    WebviewUrl::App("settings.html".into()),
                )
                .title("Blink Settings")
                .inner_size(960.0, 680.0)
                .min_inner_size(760.0, 520.0)
                .center()
                .build();
            }
        }
        BuiltinActionKind::LockWorkstation => {
            // Windows API：锁定工作站
            #[cfg(target_os = "windows")]
            unsafe {
                use windows::Win32::System::Shutdown::LockWorkStation;
                let _ = LockWorkStation();
            }
        }
        BuiltinActionKind::Shutdown => {
            // 调用 shutdown.exe 关机（/s = shutdown，/t 0 = 立即）
            #[cfg(target_os = "windows")]
            {
                let _ = std::process::Command::new("shutdown.exe")
                    .args(["/s", "/t", "0"])
                    .spawn();
            }
        }
        BuiltinActionKind::Restart => {
            // 重启
            #[cfg(target_os = "windows")]
            {
                let _ = std::process::Command::new("shutdown.exe")
                    .args(["/r", "/t", "0"])
                    .spawn();
            }
        }
        BuiltinActionKind::Sleep => {
            // 睡眠：调用 rundll32.exe powrprof.dll,SetSuspendState 0,1,0
            #[cfg(target_os = "windows")]
            {
                let _ = std::process::Command::new("rundll32.exe")
                    .args(["powrprof.dll,SetSuspendState", "0,1,0"])
                    .spawn();
            }
        }
        BuiltinActionKind::ClearHistory => {
            // 清空搜索历史
            let pool = app.state::<sqlx::SqlitePool>();
            crate::history::clear(&pool).await;
            tracing::info!("搜索历史已清空");
        }
        BuiltinActionKind::ExitBlink => {
            // 退出 Blink
            app.exit(0);
        }
        BuiltinActionKind::OpenLogs => {
            // 打开日志文件
            tracing::debug!("执行内置动作：打开日志文件");
            let log_path = crate::logging::current_log_file();
            let log_dir = crate::logging::log_dir();
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
        }
        BuiltinActionKind::OpenDataDir => {
            // 打开数据目录（APPDATA/Blink）
            tracing::debug!("执行内置动作：打开数据目录");
            if let Ok(appdata) = std::env::var("APPDATA") {
                let dir = std::path::PathBuf::from(appdata).join("Blink");
                tracing::debug!(dir = %dir.display(), "数据目录路径");
                if let Err(e) = open::that(&dir) {
                    tracing::error!(error = %e, dir = %dir.display(), "打开数据目录失败");
                }
            } else {
                tracing::error!("APPDATA 环境变量未找到");
            }
        }
    }
    Ok(())
}

/// 设置页-存储：获取历史记录统计信息。
#[tauri::command]
pub async fn get_storage_info(app: tauri::AppHandle) -> serde_json::Value {
    let pool = app.state::<sqlx::SqlitePool>();
    let count = crate::history::count(&pool).await;
    let db_path = crate::history::db_path_str();
    serde_json::json!({
        "history_count": count,
        "db_path": db_path,
    })
}

/// 设置页-存储：清空历史记录。
#[tauri::command]
pub async fn clear_history(app: tauri::AppHandle) -> Result<(), String> {
    let pool = app.state::<sqlx::SqlitePool>();
    crate::history::clear(&pool).await;
    Ok(())
}

/// 调整主窗口大小（前端调用，用于弹性窗口）。
/// 设置大小后若窗口底部超出显示器工作区，自动上移使其完整可见。
#[tauri::command]
pub async fn resize_window(app: tauri::AppHandle, width: f64, height: f64) -> Result<(), String> {
    if let Some(win) = app.get_webview_window("main") {
        let size = tauri::LogicalSize::new(width, height);
        win.set_size(size).map_err(|e| e.to_string())?;
        crate::window::clamp_to_work_area(&win);
    }
    Ok(())
}

// ── 配置相关命令 ────────────────────────────────────────────────────────────────

/// 获取完整配置。
#[tauri::command]
pub async fn get_config(app: tauri::AppHandle) -> crate::config::AppConfig {
    let pool = app.state::<sqlx::SqlitePool>();
    crate::config::get_config(&pool).await
}

/// 更新快捷键配置。
#[tauri::command]
pub async fn update_hotkey(
    app: tauri::AppHandle,
    modifiers: Vec<String>,
    key: String,
    display: String,
) -> Result<(), String> {
    let pool = app.state::<sqlx::SqlitePool>();
    let hotkey = crate::config::HotkeyConfig {
        modifiers,
        key,
        display,
    };
    // 保存到数据库
    crate::config::update_hotkey(&pool, hotkey.clone()).await?;
    // 同时更新运行时热键配置
    crate::hotkey::update_config(hotkey);
    tracing::debug!("update_hotkey: → Ok");
    Ok(())
}

/// 更新 tap 阈值。
#[tauri::command]
pub async fn update_tap_threshold(app: tauri::AppHandle, threshold: u64) -> Result<(), String> {
    let pool = app.state::<sqlx::SqlitePool>();
    crate::config::update_tap_threshold(&pool, threshold).await?;
    // 同时更新运行时热键配置
    crate::hotkey::update_tap_threshold(threshold);
    Ok(())
}

/// 更新 grace period。
#[tauri::command]
pub async fn update_grace_period(app: tauri::AppHandle, period: u64) -> Result<(), String> {
    let pool = app.state::<sqlx::SqlitePool>();
    crate::config::update_grace_period(&pool, period).await?;
    // 同时更新运行时窗口配置
    crate::window::update_grace_period(period);
    Ok(())
}

/// 更新开机自启设置：存配置 + 真正注册/注销系统开机自启（注册表 Run 项）。
#[tauri::command]
pub async fn update_auto_start(app: tauri::AppHandle, auto_start: bool) -> Result<(), String> {
    let pool = app.state::<sqlx::SqlitePool>();
    crate::config::update_auto_start(&pool, auto_start).await?;
    use tauri_plugin_autostart::ManagerExt;
    let manager = app.autolaunch();
    if auto_start {
        manager.enable().map_err(|e| e.to_string())?;
    } else {
        manager.disable().map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// 更新语言设置。
#[tauri::command]
pub async fn update_language(app: tauri::AppHandle, language: String) -> Result<(), String> {
    let pool = app.state::<sqlx::SqlitePool>();
    crate::config::update_language(&pool, language).await
}

/// 更新日志级别（存配置 + 运行时 reload，立即生效）。
#[tauri::command]
pub async fn update_log_level(app: tauri::AppHandle, level: String) -> Result<(), String> {
    let pool = app.state::<sqlx::SqlitePool>();
    crate::config::update_log_level(&pool, level.clone()).await?;
    crate::logging::update_level(&level);
    Ok(())
}

/// 打开当天日志文件（资源管理器中定位；文件不存在则打开文件夹）。
#[tauri::command]
pub fn open_log_file() -> Result<(), String> {
    let path = crate::logging::current_log_file();
    let arg = if path.exists() {
        format!("/select,{}", path.display())
    } else {
        // 当天尚无日志（如 error 级未产生），直接打开文件夹
        crate::logging::log_dir().display().to_string()
    };
    std::process::Command::new("explorer.exe")
        .arg(arg)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// 打开日志文件夹。
#[tauri::command]
pub fn open_log_dir() -> Result<(), String> {
    std::process::Command::new("explorer.exe")
        .arg(crate::logging::log_dir())
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// 获取日志路径信息（供设置页显示）。
#[tauri::command]
pub fn get_log_info() -> serde_json::Value {
    serde_json::json!({
        "dir": crate::logging::log_dir().to_string_lossy(),
        "current_file": crate::logging::current_log_file().to_string_lossy(),
    })
}

/// 恢复默认配置。
#[tauri::command]
pub async fn reset_config(app: tauri::AppHandle) -> Result<(), String> {
    let pool = app.state::<sqlx::SqlitePool>();
    let config = crate::config::AppConfig::default();
    crate::config::save_config(&pool, &config).await
}

/// 更新文件搜索配置。
#[tauri::command]
pub async fn update_file_search(
    app: tauri::AppHandle,
    enabled: bool,
    everything_port: u16,
    local_scan_depth: u32,
) -> Result<(), String> {
    let pool = app.state::<sqlx::SqlitePool>();
    let file_search = crate::config::FileSearchConfig {
        enabled,
        everything_port,
        local_scan_depth,
    };
    crate::config::update_file_search(&pool, file_search).await
}

/// 探测 Everything HTTP Server 状态。
#[tauri::command]
pub async fn probe_everything(port: u16) -> bool {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .unwrap_or_default();
    let url = format!("http://localhost:{port}/?search=__blink_probe__&json=1&count=1");
    match client.get(&url).send().await {
        Ok(resp) => resp.status().is_success(),
        Err(_) => false,
    }
}

/// 获取所有已加载插件的信息（设置页用）。
#[tauri::command]
pub async fn get_plugins(app: tauri::AppHandle) -> Vec<serde_json::Value> {
    let engine = app.state::<Option<std::sync::Arc<crate::plugin::PluginEngine>>>();
    match engine.as_ref() {
        Some(e) => e.list_plugins(),
        None => Vec::new(),
    }
}

/// 获取引擎配置（通用 API）。
#[tauri::command]
pub async fn get_engine_config(
    app: tauri::AppHandle,
    engine_id: String,
) -> Result<serde_json::Value, String> {
    let pool = app.state::<sqlx::SqlitePool>();
    Ok(crate::config::get_engine_config(&pool, &engine_id)
        .await
        .unwrap_or_else(|| serde_json::json!({})))
}

/// 更新引擎配置（通用 API）。
#[tauri::command]
pub async fn update_engine_config(
    app: tauri::AppHandle,
    engine_id: String,
    config: serde_json::Value,
) -> Result<(), String> {
    let pool = app.state::<sqlx::SqlitePool>();
    crate::config::set_engine_config(&pool, &engine_id, &config).await?;

    // 通知引擎配置更新（FileEngine 需要重新探测端口等）
    tracing::debug!(engine_id, "引擎配置已更新");
    Ok(())
}

/// 录制快捷键（阻塞，直到用户按下组合键或超时）。
#[tauri::command]
pub async fn record_hotkey() -> Result<serde_json::Value, String> {
    // 在阻塞线程中等待录制（事件由 ll_proc 喂入 recorder 状态机）
    let result = tokio::task::spawn_blocking(|| {
        crate::hotkey::record_hotkey_blocking()
    })
    .await
    .map_err(|e| e.to_string())?;

    match result {
        Some(record) => {
            let val = serde_json::json!({
                "modifiers": record.modifiers,
                "key": record.key,
                "display": record.display,
            });
            tracing::debug!("record_hotkey: → Ok display={}", record.display);
            Ok(val)
        }
        None => {
            tracing::warn!("record_hotkey: → Err (None)");
            Err("录制超时或取消".to_string())
        }
    }
}
