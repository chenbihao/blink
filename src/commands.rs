//! Tauri command 层：前端 invoke 入口，组合 core/search/history 能力。
//!
//! 命令保持轻量——编排逻辑，不含业务实现。

use tauri::Manager;

/// 前端 ESC 调用：隐藏窗口。
#[tauri::command]
pub fn hide_window(app: tauri::AppHandle) {
    crate::window::hide(&app, "ESC");
}

/// 前端输入时调用：先尝试实时计算，失败再模糊搜索开始菜单（融合历史权重）。
///
// TODO: 搜索缓存 — 每次调用都重新扫描开始菜单，频繁输入时可能卡顿。
//   后续拆分为独立搜索引擎：开始菜单搜索、快捷方式搜索、文件搜索、意图识别等，
//   每个引擎独立缓存/增量更新，commands 层只做路由和结果合并。
#[tauri::command]
pub async fn search_apps(query: String, app: tauri::AppHandle) -> Vec<crate::search::AppEntry> {
    // 优先尝试实时计算（如 "1+1" → "2"）
    if let Some(result) = crate::calc::try_eval(&query) {
        return vec![crate::search::AppEntry {
            name: format!("= {result}"),
            pinyin_name: String::new(),
            lnk_path: String::new(),
            is_calc: true,
        }];
    }
    // 走搜索（融合历史权重）
    let entries = crate::search::scan_start_menu();
    let pool = app.state::<sqlx::SqlitePool>();
    let history = crate::history::get_weights(&pool).await;
    crate::search::fuzzy_search(&query, &entries, &history, 10)
}

/// 前端回车/点击时调用：启动选中的应用（打开 lnk）并记录历史。
/// 计算结果无 lnk_path，忽略。
#[tauri::command]
pub async fn launch_app(app: tauri::AppHandle, lnk_path: String) -> Result<(), String> {
    if lnk_path.is_empty() {
        return Ok(());
    }
    let pool = app.state::<sqlx::SqlitePool>();
    crate::history::record_launch(&pool, &lnk_path).await;
    crate::search::launch(&lnk_path)?;
    crate::window::hide(&app, "launch");
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
#[tauri::command]
pub async fn resize_window(app: tauri::AppHandle, width: f64, height: f64) -> Result<(), String> {
    if let Some(win) = app.get_webview_window("main") {
        let size = tauri::LogicalSize::new(width, height);
        win.set_size(size).map_err(|e| e.to_string())?;
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
    eprintln!("[cmd:update_hotkey] called key={}", key);
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
    eprintln!("[cmd:update_hotkey] → Ok");
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

/// 更新开机自启设置。
#[tauri::command]
pub async fn update_auto_start(app: tauri::AppHandle, auto_start: bool) -> Result<(), String> {
    let pool = app.state::<sqlx::SqlitePool>();
    crate::config::update_auto_start(&pool, auto_start).await
}

/// 更新语言设置。
#[tauri::command]
pub async fn update_language(app: tauri::AppHandle, language: String) -> Result<(), String> {
    let pool = app.state::<sqlx::SqlitePool>();
    crate::config::update_language(&pool, language).await
}

/// 恢复默认配置。
#[tauri::command]
pub async fn reset_config(app: tauri::AppHandle) -> Result<(), String> {
    let pool = app.state::<sqlx::SqlitePool>();
    let config = crate::config::AppConfig::default();
    crate::config::save_config(&pool, &config).await
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
            eprintln!("[cmd:record_hotkey] → Ok display={}", record.display);
            Ok(val)
        }
        None => {
            eprintln!("[cmd:record_hotkey] → Err (None)");
            Err("录制超时或取消".to_string())
        }
    }
}
