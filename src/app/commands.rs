//! Tauri command 层：前端 invoke 入口，组合 core/search/history 能力。
//!
//! 命令保持轻量——编排逻辑，不含业务实现。

use tauri::{Emitter, Manager};
use tauri_plugin_dialog::DialogExt;

/// 打开文件选择对话框，返回选中的文件路径（取消时返回 null）。
#[tauri::command]
pub async fn open_file_dialog(
    app: tauri::AppHandle,
    title: String,
    filters: Vec<serde_json::Value>,
) -> Option<String> {
    // 构造过滤器
    let mut dialog = app.dialog().file();
    if !title.is_empty() {
        dialog = dialog.set_title(title);
    }
    // 转换过滤器格式（简化处理，只取第一个扩展名）
    for filter in filters {
        if let Some(name) = filter.get("name").and_then(|v| v.as_str()) {
            if let Some(exts) = filter.get("extensions").and_then(|v| v.as_array()) {
                let extensions: Vec<&str> = exts.iter().filter_map(|e| e.as_str()).collect();
                if !extensions.is_empty() {
                    dialog = dialog.add_filter(name, &extensions);
                }
            }
        }
    }
    dialog.blocking_pick_file().and_then(|p| match p {
        tauri_plugin_dialog::FilePath::Path(path) => path.to_str().map(|s| s.to_string()),
        tauri_plugin_dialog::FilePath::Url(url) => Some(url.to_string()),
    })
}

/// 主窗口 ESC 调用：隐藏主窗口。
#[tauri::command]
pub fn hide_window(app: tauri::AppHandle) {
    crate::infra::platform::window::hide(&app, "ESC");
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
///
/// 0.8.3 §4.3：返回契约 `SearchResponse { entries, suggestion }`——
/// Keyword（0.8.1 输入补全）与 Context（0.8.3 环境感知）Ghost 走同一字段。
#[tauri::command]
pub async fn search_apps(
    query: String,
    seq: u64,
    app: tauri::AppHandle,
) -> crate::domain::search::SearchResponse {
    tracing::debug!(%query, seq, "search_apps: 收到搜索请求");
    let service = app.state::<std::sync::Arc<crate::domain::search::SearchService>>();
    let results = service.search(&query, seq).await;
    tracing::debug!(
        count = results.entries.len(),
        has_suggestion = results.suggestion.is_some(),
        %query,
        "search_apps: 返回结果"
    );
    for (i, item) in results.entries.iter().enumerate() {
        let detail = item.score_detail.as_deref().unwrap_or("");
        tracing::trace!(
            index = i,
            score = if detail.is_empty() {
                format!("{:.4}", item.score)
            } else {
                format!("{:.4} ({})", item.score, detail)
            },
            source = %item.source,
            name = %item.name,
            lnk_path = %item.lnk_path,
            "搜索结果项"
        );
    }
    if let Some(sug) = &results.suggestion {
        tracing::debug!(
            display = %sug.display,
            replacement = %sug.replacement,
            source = ?sug.source,
            confidence = sug.confidence,
            "suggestion"
        );
    }
    results
}

/// 前端回车/点击时调用：启动选中的应用（普通 lnk 路径）。
///
/// 0.8.0 §1.3 起，内置动作走 `run_builtin_action`（前端 `Action.kind == "run"` 时分派），
/// 此命令只处理真正的文件/应用路径。计算结果无 lnk_path，忽略。
#[tauri::command]
pub async fn launch_app(app: tauri::AppHandle, lnk_path: String) -> Result<(), String> {
    if lnk_path.is_empty() {
        return Ok(());
    }

    tracing::debug!(%lnk_path, "launch_app: 普通应用启动");

    let pool = app.state::<sqlx::SqlitePool>();
    // search_history_enabled=false 时跳过记录（隐私/偏好）；该项频率加权随之失效
    let config = crate::app::config::get_config(&pool).await;
    if config.search_history_enabled {
        crate::infra::data::history::record_launch(&pool, &lnk_path).await;
    }
    crate::domain::search::launch(&lnk_path)?;
    crate::infra::platform::window::hide(&app, "launch");
    Ok(())
}

// 0.8.6 重构：execute_builtin_action 已迁移到 domain::execution 的各 Action struct。
// run_builtin_action 现在通过 ActionRegistry 查找并执行。

/// 运行内置动作（0.8.0 §1.3 / 0.8.6 §8.1.1 重构）。
///
/// 前端 `Action.kind === "run"` → `invoke("run_builtin_action", { id, arg })`。
/// `id` 为内置动作注册表 key（如 `"open_settings"`），后端按 id 从 `ActionRegistry` 查找
/// 对应的 `Action` 实现并执行。
///
/// 0.8.6 重构：原 `BuiltinActionKind` match 分支迁移到 `domain::execution` 的
/// 各 `Action` struct 实现，本函数变为薄委托层。
///
/// 未知 id → 返回 `Err`；前端会打印到控制台，不弹窗。
#[tauri::command]
pub async fn run_builtin_action(
    app: tauri::AppHandle,
    id: String,
    arg: Option<serde_json::Value>,
) -> Result<(), String> {
    tracing::debug!(%id, ?arg, "run_builtin_action: 收到请求");

    let registry = app.state::<std::sync::Arc<crate::domain::execution::ActionRegistry>>();
    let Some(action) = registry.get(&id) else {
        let msg = format!("未知内置动作 id: {id}");
        tracing::warn!(%id, "run_builtin_action: 未知 id");
        return Err(msg);
    };

    let cx = crate::domain::execution::ActionContext::new(&app, arg);
    match action.execute(&cx).await {
        Ok(_outcome) => {
            // 内置动作全部返回 Nop；outcome 为未来扩展预留
        }
        Err(e) => {
            tracing::error!(%id, error = %e, "内置动作执行失败");
            return Err(e.to_string());
        }
    }

    // 所有内置动作都隐藏主窗口；设置窗口在 OpenSettings 分支里已单独显示。
    crate::infra::platform::window::hide(&app, "run_builtin_action");
    Ok(())
}

/// 列出所有内置动作元数据 + 当前 enabled 状态（0.8.0 §1.3 / 0.8.6 §8.2.4 i18n）。
#[tauri::command]
pub async fn list_builtin_actions(
    app: tauri::AppHandle,
) -> Vec<crate::domain::search::BuiltinActionInfo> {
    let pool = app.state::<sqlx::SqlitePool>();
    let disabled = crate::app::config::get_disabled_builtin_actions(&pool).await;
    let registry = app.state::<std::sync::Arc<crate::domain::execution::ActionRegistry>>();
    // 读当前语言（从 AppConfig 快照取）
    let config = crate::app::config::get_config(&pool).await;
    crate::domain::search::list_builtin_actions(&disabled, &registry, &config.language)
}


/// 触发 Chord 动作（0.8.5 §六）。前端 Alt+字母 → invoke 此 command。
///
/// key 为字母（不区分大小写）。未注册 → Err（前端 log，不弹窗）。
///
/// **surface 分派**（0.8.5 §6.4 简化后）：
/// - `MiniBall` (Alt+Q 划词)：hide 主窗 + show chord-ball 悬浮窗
/// - `Screenshot` (Alt+A) / `Default` / 其它：action 自己在 execute 内决定 UI 反馈
///   （如 ClipboardHistoryAction 自 emit fill-query），command 层不再统一后处理
///
/// **门禁**（0.8.7 修复）：设置页取消勾选的 Chord 动作 → disabled 列表命中即静默早退。
/// 前端 `list_chord_actions` 已过滤 disabled 只是让"提示条不显示"，但 Alt+字母 触发是
/// **另一条独立路径**（前端 keyboard.js 直接按物理键 invoke），必须在 command 层守门。
#[tauri::command]
pub async fn trigger_chord(app: tauri::AppHandle, key: String) -> Result<(), String> {
    tracing::debug!(%key, "trigger_chord");
    let Some(registry) = app.try_state::<std::sync::Arc<crate::domain::chord::ChordRegistry>>() else {
        return Err("chord registry 未就绪".into());
    };
    // 查该 key 对应的 action id,若在 disabled 列表 → 早退
    let key_lower = key.to_lowercase();
    if let Some(action_id) = registry.action_id_for_key(&key_lower) {
        let pool = app.state::<sqlx::SqlitePool>();
        let disabled = crate::app::config::get_disabled_chord_actions(&pool).await;
        if disabled.iter().any(|d| d == action_id) {
            tracing::debug!(%key_lower, %action_id, "chord 已禁用,跳过触发");
            return Ok(());
        }
    }
    let surface = registry.trigger(&key, &app).await?;
    // surface=MiniBall → 显示 chord-ball 悬浮窗（划词指示，不抢焦点）
    if surface == crate::domain::chord::ChordSurface::MiniBall {
        // 隐藏主窗（球作划词指示，主窗不打扰）+ 显示悬浮球
        crate::infra::platform::window::hide(&app, "chord");
        crate::infra::platform::window::show_chord_ball(&app)?;
    }
    Ok(())
}

/// 隐藏 chord-ball 悬浮窗（悬浮球内点击/ESC 调）。
#[tauri::command]
pub fn hide_chord_ball(app: tauri::AppHandle) {
    crate::infra::platform::window::hide_chord_ball(&app);
}

/// 确认截图选区（0.8.7）：前端拖选完成后回调。
///
/// - `x/y/w/h`：选区在虚拟屏幕上的**物理像素**坐标（前端已乘 DPR）。
/// - 从 SESSION 裁剪 RGBA → 写入剪贴板 CF_DIB → 隐藏 overlay + 清 SESSION。
///
/// 裁剪逻辑走 `screenshot::crop`（越界自动 clamp），PNG 编解码不再来回一遍。
#[tauri::command]
pub async fn capture_region(
    app: tauri::AppHandle,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
) -> Result<(), String> {
    let (bgra, cw, ch) = crate::infra::platform::screenshot::crop(x, y, w, h)
        .ok_or_else(|| "截图会话为空或选区越界".to_string())?;

    crate::infra::platform::clipboard::write_bgra_to_clipboard(&bgra, cw, ch)?;

    // 隐藏 overlay + 清空 SESSION（hide_screenshot_overlay 内部会 end_session）
    crate::infra::platform::window::hide_screenshot_overlay(&app);

    tracing::info!(x, y, w = cw, h = ch, "截图已保存到剪贴板");
    Ok(())
}

/// 隐藏截图覆盖窗（ESC / 失焦 / 选区过小时调）。
#[tauri::command]
pub fn hide_screenshot_overlay(app: tauri::AppHandle) {
    crate::infra::platform::window::hide_screenshot_overlay(&app);
}

/// 轮询选区缓存（chord-ball 前端 200ms 轮询，检测划词是否成功）。
/// 返回 Some(text) 表示选区已就绪；None 表示尚未抓到或已过期。
#[tauri::command]
pub fn poll_chord_selection() -> Option<String> {
    crate::infra::platform::selection::get_last_selection()
}

/// 确认划词（0.8.5 §6.5）：读 selection 缓存 → emit 到主窗 → 主窗 show + 球 hide。
/// 主窗前端 listen `blink://chord-translate` 填搜索框「翻译 {text}」触发翻译插件。
#[tauri::command]
pub async fn confirm_chord_selection(app: tauri::AppHandle) -> Result<(), String> {
    let text = crate::infra::platform::selection::get_last_selection();
    crate::infra::platform::window::hide_chord_ball(&app);
    crate::infra::platform::window::invoke(&app);
    if let Some(t) = text {
        let _ = app.emit("blink://chord-translate", t);
    }
    Ok(())
}

/// 列出所有已注册的 Chord 动作元数据（0.8.5 §六 Ghost overlay 提示层渲染用）。
///
/// 每条：`{ id, key, label, surface }`。已 disabled 的跳过；label 按当前 UI 语言解析。
#[tauri::command]
pub async fn list_chord_actions(app: tauri::AppHandle) -> Vec<serde_json::Value> {
    let pool = app.state::<sqlx::SqlitePool>();
    let disabled = crate::app::config::get_disabled_chord_actions(&pool).await;
    let language = crate::app::config::get_config(&pool).await.language;
    let Some(registry) = app.try_state::<std::sync::Arc<crate::domain::chord::ChordRegistry>>() else {
        return Vec::new();
    };
    registry.list(&disabled, &language)
}

/// 列出所有已注册的 Chord 动作 + enabled 状态（0.8.5.1 §6.6 设置页用）。
///
/// 与 `list_chord_actions` 的区别:不过滤 disabled,而是每条附带 `enabled` 字段。
/// 用于设置页展示"所有可开关的动作",让用户能勾选禁用。
#[tauri::command]
pub async fn list_all_chord_actions(app: tauri::AppHandle) -> Vec<serde_json::Value> {
    let pool = app.state::<sqlx::SqlitePool>();
    let disabled = crate::app::config::get_disabled_chord_actions(&pool).await;
    let language = crate::app::config::get_config(&pool).await.language;
    let Some(registry) = app.try_state::<std::sync::Arc<crate::domain::chord::ChordRegistry>>() else {
        return Vec::new();
    };
    registry.list_all(&disabled, &language)
}

/// 当前 Alt 键是否物理按下（0.8.5 §6.1）。前端轮询驱动 alt-active 状态——
/// WebView2 不转发 Alt 键自身的 keydown 到 JS，前端监听不可靠，改轮询物理态。
#[tauri::command]
pub fn is_alt_down() -> bool {
    crate::infra::platform::hotkey::is_alt_down()
}


/// 列出所有已注册的 context binding + 当前 enabled 状态（0.8.3 §4.6 设置页面板）。
///
/// 每条 binding 描述：`{ key, target_id, trigger_key, target_label, trigger_label, enabled }`。
/// - `key`：`{target_id}::{trigger_key}`，作 disable 列表存储项
/// - `target_label`：从 PluginManifest.name 本地化（缺失时降级 target_id）
/// - `trigger_label`：显示名（如「文本非目标语言 → 翻译」），i18n key（前端翻）
/// - `enabled`：用户配置的启用状态
#[tauri::command]
pub async fn list_context_bindings(app: tauri::AppHandle) -> Vec<serde_json::Value> {
    let pool = app.state::<sqlx::SqlitePool>();
    let config = crate::app::config::get_config(&pool).await;
    let disabled: std::collections::HashSet<String> =
        config.disabled_context_bindings.iter().cloned().collect();
    let lang = config.language.clone();

    // 从 PluginEngine 拉所有插件的 manifest.triggers 里的 Context 变体。
    // PluginEngine 现在为 `Arc<PluginEngine>`（空插件也构造，见 main.rs），无需 Option 处理。
    let Some(pe) = app.try_state::<std::sync::Arc<crate::domain::plugin::PluginEngine>>() else {
        return Vec::new();
    };
    let mut bindings = Vec::new();
    for manifest in pe.list_manifests() {
        for trigger in &manifest.triggers {
            if let crate::domain::plugin::PluginTrigger::Context { when, .. } = trigger {
                let ctx_when: crate::domain::context::trigger::ContextTrigger = (*when).into();
                let trigger_key = crate::domain::intent::trigger_key(&ctx_when);
                let key = crate::domain::intent::binding_key(&manifest.id, trigger_key);
                let target_label = manifest.name.resolve(&lang);
                let enabled = !disabled.contains(&key);
                bindings.push(serde_json::json!({
                    "key": key,
                    "target_id": manifest.id,
                    "trigger_key": trigger_key,
                    "target_label": target_label,
                    "trigger_label": trigger_key, // 前端按 key 翻译（i18n）
                    "enabled": enabled,
                }));
            }
        }
    }
    bindings
}

/// 设置页-存储：获取历史记录统计信息。
#[tauri::command]
pub async fn get_storage_info(app: tauri::AppHandle) -> serde_json::Value {
    let pool = app.state::<sqlx::SqlitePool>();
    let count = crate::infra::data::history::count(&pool).await;
    let db_path = crate::infra::data::history::db_path_str();
    serde_json::json!({
        "history_count": count,
        "db_path": db_path,
    })
}

/// 设置页-关于：应用元信息（版本/名称/描述/仓库）。
/// 版本从 Cargo.toml 编译期注入（`CARGO_PKG_*`），tauri.conf.json 版本单独在 bundle 层使用。
/// 保持这两处同步：升版本时改 Cargo.toml + tauri.conf.json 两个地方。
#[tauri::command]
pub fn get_app_info() -> serde_json::Value {
    serde_json::json!({
        "name": env!("CARGO_PKG_NAME"),
        "version": env!("CARGO_PKG_VERSION"),
        "description": env!("CARGO_PKG_DESCRIPTION"),
        "license": env!("CARGO_PKG_LICENSE"),
        "repository": env!("CARGO_PKG_REPOSITORY"),
    })
}

/// 设置页-关于：检查 GitHub 最新 Release 版本。
/// 返回 `{ has_update, current_version, latest_version, release_url }`。
/// 网络失败或解析异常时静默返回 `has_update: false`（不打扰用户）。
#[tauri::command]
pub async fn check_update() -> serde_json::Value {
    let current = env!("CARGO_PKG_VERSION");
    let repo = env!("CARGO_PKG_REPOSITORY");
    // 从 "https://github.com/owner/repo" 提取 "owner/repo"
    let repo_path = repo
        .trim_start_matches("https://github.com/")
        .trim_start_matches("http://github.com/")
        .trim_end_matches('/');

    let api_url = format!("https://api.github.com/repos/{repo_path}/releases/latest");

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .user_agent("blink-updater")
        .build()
        .unwrap_or_default();

    let Ok(resp) = client.get(&api_url).send().await else {
        tracing::debug!("check_update: 请求 GitHub API 失败");
        return serde_json::json!({ "has_update": false, "current_version": current });
    };
    if !resp.status().is_success() {
        tracing::debug!("check_update: GitHub API 返回 {}", resp.status());
        return serde_json::json!({ "has_update": false, "current_version": current });
    }
    let Ok(body) = resp.json::<serde_json::Value>().await else {
        tracing::debug!("check_update: 解析 JSON 失败");
        return serde_json::json!({ "has_update": false, "current_version": current });
    };

    let tag = body["tag_name"].as_str().unwrap_or("");
    let latest = tag.trim_start_matches('v');
    let release_url = body["html_url"]
        .as_str()
        .unwrap_or(&format!("https://github.com/{repo_path}/releases/latest"))
        .to_string();

    let has_update = version_gt(latest, current);
    if has_update {
        tracing::info!("发现新版本: {current} -> {latest}");
    }

    serde_json::json!({
        "has_update": has_update,
        "current_version": current,
        "latest_version": latest,
        "release_url": release_url,
    })
}

/// 语义化版本比较：a > b 则返回 true。
fn version_gt(a: &str, b: &str) -> bool {
    let parse = |s: &str| -> (u64, u64, u64) {
        let parts: Vec<u64> = s.split('.').filter_map(|p| p.parse().ok()).collect();
        (
            parts.first().copied().unwrap_or(0),
            parts.get(1).copied().unwrap_or(0),
            parts.get(2).copied().unwrap_or(0),
        )
    };
    parse(a) > parse(b)
}

/// 设置页-存储：清空历史记录。
#[tauri::command]
pub async fn clear_history(app: tauri::AppHandle) -> Result<(), String> {
    let pool = app.state::<sqlx::SqlitePool>();
    crate::infra::data::history::clear(&pool).await;
    Ok(())
}

/// 调整主窗口大小（前端调用，用于弹性窗口）。
/// 设置大小后若窗口底部超出显示器工作区，自动上移使其完整可见。
#[tauri::command]
pub async fn resize_window(app: tauri::AppHandle, width: f64, height: f64) -> Result<(), String> {
    if let Some(win) = app.get_webview_window("main") {
        let size = tauri::LogicalSize::new(width, height);
        win.set_size(size).map_err(|e| e.to_string())?;
        crate::infra::platform::window::clamp_to_work_area(&win);
    }
    Ok(())
}

// ── 配置相关命令 ────────────────────────────────────────────────────────────────

/// 获取完整配置。
#[tauri::command]
pub async fn get_config(app: tauri::AppHandle) -> crate::app::config::AppConfig {
    let pool = app.state::<sqlx::SqlitePool>();
    crate::app::config::get_config(&pool).await
}

/// 泛型配置写入（0.8.6 P1-C 前端泛型化）。
///
/// 前端统一调用 `invoke('set_config', { key, value })`，后端按 key 路由到
/// 对应分片持久化 + 副作用（SearchService 热更新 / 平台 API / emit 事件）。
///
/// # 支持的 key
///
/// **AppConfig 分片**：`language` / `log_level` / `auto_start` / `hotkey` /
/// `tap_threshold` / `grace_period` / `general_config` / `autosuggest` /
/// `chord_toggles` / `clipboard_enabled` / `disabled_builtin_actions` /
/// `disabled_context_bindings` / `disabled_chord_actions`
///
/// **引擎配置**：`file_search` / `start_menu_config` / `calc_config` / `global_proxy`
///
/// **插件配置**：`plugin_config`
///
/// **Context 配置**：`context_config`
#[tauri::command]
pub async fn set_config(
    app: tauri::AppHandle,
    key: String,
    value: serde_json::Value,
) -> Result<(), String> {
    let pool = app.state::<sqlx::SqlitePool>();

    match key.as_str() {
        // ── 单值字段（直接解析） ──────────────────────────────────────────
        "language" => {
            let language: String = serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::config::update_language(&pool, language.clone()).await?;
            if let Some(ss) = app.try_state::<std::sync::Arc<crate::domain::search::SearchService>>() {
                ss.update_language(language.clone());
            }
            let _ = app.emit("blink://config-changed", ());
            tracing::info!(%language, "语言已更新");
        }
        "log_level" => {
            let level: String = serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::config::update_log_level(&pool, level.clone()).await?;
            crate::infra::utils::logging::update_level(&level);
            tracing::info!(%level, "日志级别已切换");
        }
        "auto_start" => {
            let auto_start: bool = serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::config::update_auto_start(&pool, auto_start).await?;
            use tauri_plugin_autostart::ManagerExt;
            let manager = app.autolaunch();
            if auto_start { manager.enable().map_err(|e| e.to_string())?; }
            else { manager.disable().map_err(|e| e.to_string())?; }
            tracing::info!(auto_start, "开机自启配置已更新");
        }
        "tap_threshold" => {
            let threshold: u64 = serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::config::update_tap_threshold(&pool, threshold).await?;
            crate::infra::platform::hotkey::update_tap_threshold(threshold);
            tracing::debug!(threshold, "tap 阈值已更新");
        }
        "grace_period" => {
            let period: u64 = serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::config::update_grace_period(&pool, period).await?;
            crate::infra::platform::window::update_grace_period(period);
            tracing::debug!(period, "grace period 已更新");
        }
        "clipboard_enabled" => {
            let enabled: bool = serde_json::from_value(value).map_err(|e| e.to_string())?;
            // 只读写 clipboard:config 分片，不走门面 get_config（避免 7 片全读全写）。
            // 0.9 删掉 AppConfig.clipboard 字段后此处不受影响。
            let mut clip_cfg = crate::app::config::ConfigStore::get::<crate::infra::data::clipboard::ClipboardConfig>(&pool).await;
            clip_cfg.enabled = enabled;
            crate::app::config::ConfigStore::set(&pool, &clip_cfg).await?;
            crate::infra::platform::clipboard::set_active(enabled);
            let _ = app.emit("blink://config-changed", ());
            tracing::info!(enabled, "剪贴板监听开关已更新");
        }

        // ── 结构体字段（按 key 对应 serde 解析） ──────────────────────────
        "hotkey" => {
            let hotkey: crate::app::config::HotkeyConfig =
                serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::config::update_hotkey(&pool, hotkey.clone()).await?;
            crate::infra::platform::hotkey::update_config(hotkey.clone());
            tracing::info!(display = %hotkey.display, "全局热键已更新");
        }
        "general_config" => {
            let general: crate::app::config::GeneralConfig =
                serde_json::from_value(value).map_err(|e| e.to_string())?;
            let max_results = general.max_results;
            crate::app::config::update_general_config(&pool, &general).await?;
            if let Some(ss) = app.try_state::<std::sync::Arc<crate::domain::search::SearchService>>() {
                ss.update_max_results(max_results as usize);
            }
            let _ = app.emit("blink://config-changed", ());
            tracing::info!(
                theme = %general.theme,
                search_history_enabled = general.search_history_enabled,
                search_history_days = general.search_history_days,
                max_results,
                page_size = general.page_size,
                "通用配置已更新"
            );
        }
        "autosuggest" => {
            let v: crate::app::config::AutosuggestUpdate =
                serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::config::update_autosuggest_config(
                &pool, v.enabled, v.min_score, v.tab_key.clone(),
            ).await?;
            if let Some(ss) = app.try_state::<std::sync::Arc<crate::domain::search::SearchService>>() {
                ss.update_autosuggest_config(v.enabled, v.min_score);
            }
            tracing::info!(v.enabled, v.min_score, tab_key = %v.tab_key, "Autosuggest 配置已更新");
        }
        "chord_toggles" => {
            let v: crate::app::config::ChordTogglesUpdate =
                serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::config::update_chord_toggles(
                &pool, v.chord_enabled, v.chord_hint_visible,
            ).await?;
            let _ = app.emit("blink://config-changed", ());
            tracing::info!(v.chord_enabled, v.chord_hint_visible, "Chord 开关已更新");
        }

        // ── Disable 列表 ──────────────────────────────────────────────────
        "disabled_builtin_actions" => {
            let disabled: Vec<String> = serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::config::update_disabled_builtin_actions(&pool, disabled.clone()).await?;
            let search_service = app.state::<std::sync::Arc<crate::domain::search::SearchService>>();
            search_service.update_disabled_builtin_actions(disabled.clone());
            tracing::info!(count = disabled.len(), ?disabled, "内置动作禁用列表已更新");
        }
        "disabled_context_bindings" => {
            let disabled: Vec<String> = serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::config::update_disabled_context_bindings(&pool, disabled.clone()).await?;
            if let Some(ss) = app.try_state::<std::sync::Arc<crate::domain::search::SearchService>>() {
                ss.update_disabled_context_bindings(disabled.clone());
            }
            tracing::info!(count = disabled.len(), ?disabled, "Context binding 禁用列表已更新");
        }
        "disabled_chord_actions" => {
            let disabled: Vec<String> = serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::config::update_disabled_chord_actions(&pool, disabled.clone()).await?;
            let _ = app.emit("blink://config-changed", ());
            tracing::info!(count = disabled.len(), ?disabled, "Chord 动作禁用列表已更新");
        }

        // ── 引擎配置 ──────────────────────────────────────────────────────
        "file_search" => {
            let fs: crate::app::config::FileSearchConfig =
                serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::config::update_file_search(&pool, fs.clone()).await?;
            if let Some(ss) = app.try_state::<std::sync::Arc<crate::domain::search::SearchService>>() {
                ss.update_engine_config("file", crate::domain::search::EngineConfigUpdate::File(fs.clone())).await;
            }
            tracing::info!(enabled = fs.enabled, data_source = %fs.data_source, "文件搜索配置已更新");
        }
        "start_menu_config" => {
            let sm: crate::app::config::StartMenuConfig =
                serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::config::update_start_menu_config(&pool, &sm).await?;
            if let Some(ss) = app.try_state::<std::sync::Arc<crate::domain::search::SearchService>>() {
                ss.update_engine_config("start_menu", crate::domain::search::EngineConfigUpdate::StartMenu(sm.clone())).await;
            }
            tracing::info!(enabled = sm.enabled, scan_depth = sm.scan_depth, "应用搜索配置已更新");
        }
        "calc_config" => {
            let cc: crate::app::config::CalcConfig =
                serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::config::update_calc_config(&pool, &cc).await?;
            if let Some(ss) = app.try_state::<std::sync::Arc<crate::domain::search::SearchService>>() {
                ss.update_engine_config("calc", crate::domain::search::EngineConfigUpdate::Calc(cc.clone())).await;
            }
            tracing::info!(enabled = cc.enabled, "计算器配置已更新");
        }
        "global_proxy" => {
            let v: crate::app::config::GlobalProxyUpdate =
                serde_json::from_value(value).map_err(|e| e.to_string())?;
            let config = serde_json::json!({ "http": v.http, "https": v.https });
            crate::app::config::set_engine_config(&pool, "_global_proxy", &config).await?;
            let engine = app.state::<std::sync::Arc<crate::domain::plugin::PluginEngine>>();
            let has_http = !v.http.is_empty();
            let has_https = !v.https.is_empty();
            let proxy = if !has_http && !has_https { None } else { Some((v.http, v.https)) };
            engine.update_global_proxy(proxy).await;
            tracing::info!(has_http, has_https, "全局代理配置已更新");
        }

        // ── 插件配置 ──────────────────────────────────────────────────────
        "plugin_config" => {
            let v: crate::app::config::PluginConfigUpdate =
                serde_json::from_value(value).map_err(|e| e.to_string())?;
            let engine = app.state::<std::sync::Arc<crate::domain::plugin::PluginEngine>>();
            let router = app.state::<std::sync::Arc<crate::domain::intent::RuleRouter>>();
            let mut config = engine.get_config(&v.plugin_id).unwrap_or_default();
            config.enabled = v.enabled;
            config.settings = v.settings;
            let result = engine.update_config(&v.plugin_id, config, Some(&router)).await;
            match &result {
                Ok(_) => tracing::info!(plugin_id = %v.plugin_id, enabled = v.enabled, "插件配置已更新"),
                Err(err) => tracing::warn!(plugin_id = %v.plugin_id, error = %err, "插件配置更新失败"),
            }
            result?;
        }

        // ── Context 配置 ──────────────────────────────────────────────────
        "context_config" => {
            let ctx: crate::app::config::ContextConfig =
                serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::config::set_context_config(&pool, &ctx).await?;
            crate::infra::platform::selection::set_active(ctx.selection_enabled);
            crate::infra::platform::selection::set_sensitive_apps(ctx.sensitive_apps.clone());
            if let Some(mem) = app.try_state::<std::sync::Arc<std::sync::RwLock<crate::app::config::ContextConfig>>>() {
                *mem.write().unwrap() = ctx;
            }
            tracing::debug!("Context 配置已更新");
        }

        _ => {
            return Err(format!("未知的配置 key: {key}"));
        }
    }

    Ok(())
}

/// 打开当天日志文件（资源管理器中定位；文件不存在则打开文件夹）。
#[tauri::command]
pub fn open_log_file() -> Result<(), String> {
    let path = crate::infra::utils::logging::current_log_file();
    let arg = if path.exists() {
        format!("/select,{}", path.display())
    } else {
        // 当天尚无日志（如 error 级未产生），直接打开文件夹
        crate::infra::utils::logging::log_dir().display().to_string()
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
        .arg(crate::infra::utils::logging::log_dir())
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// 获取日志路径信息（供设置页显示）。
#[tauri::command]
pub fn get_log_info() -> serde_json::Value {
    serde_json::json!({
        "dir": crate::infra::utils::logging::log_dir().to_string_lossy(),
        "current_file": crate::infra::utils::logging::current_log_file().to_string_lossy(),
    })
}

/// 恢复默认配置。
#[tauri::command]
pub async fn reset_config(app: tauri::AppHandle) -> Result<(), String> {
    let pool = app.state::<sqlx::SqlitePool>();
    let config = crate::app::config::AppConfig::default();
    crate::app::config::save_config(&pool, &config).await
}

/// 获取应用搜索配置。
#[tauri::command]
pub async fn get_start_menu_config(app: tauri::AppHandle) -> crate::app::config::StartMenuConfig {
    let pool = app.state::<sqlx::SqlitePool>();
    crate::app::config::get_start_menu_config(&pool).await
}

/// 获取计算器配置。
#[tauri::command]
pub async fn get_calc_config(app: tauri::AppHandle) -> crate::app::config::CalcConfig {
    let pool = app.state::<sqlx::SqlitePool>();
    crate::app::config::get_calc_config(&pool).await
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

/// 获取所有已加载插件的信息（设置页用）。已含 enabled + settings（0.5.1）。
#[tauri::command]
pub async fn get_plugins(app: tauri::AppHandle) -> Vec<serde_json::Value> {
    let engine = app.state::<std::sync::Arc<crate::domain::plugin::PluginEngine>>();
    // 读当前语言,供 manifest 配置文案按 locale 取值(设置页中英双语)
    let pool = app.state::<sqlx::SqlitePool>();
    let lang = crate::app::config::get_config(&pool).await.language;
    engine.list_plugins(&lang)
}

/// 禁用/恢复某个默认触发词。
#[tauri::command]
pub async fn toggle_default_trigger(
    app: tauri::AppHandle,
    plugin_id: String,
    keyword: String,
    disabled: bool,
) -> Result<(), String> {
    let engine = app.state::<std::sync::Arc<crate::domain::plugin::PluginEngine>>();
    let router = app.state::<std::sync::Arc<crate::domain::intent::RuleRouter>>();

    // 读取现有配置
    let mut config = engine.get_config(&plugin_id).unwrap_or_default();

    if disabled {
        // 加入禁用列表
        if !config.disabled_default_triggers.contains(&keyword) {
            config.disabled_default_triggers.push(keyword.clone());
        }
    } else {
        // 从禁用列表移除
        config.disabled_default_triggers.retain(|k| k != &keyword);
    }

    engine.update_config(&plugin_id, config, Some(&router)).await?;
    tracing::info!(plugin_id, keyword, disabled, "默认触发词状态已更新");
    Ok(())
}

/// 添加一个自定义触发词。
#[tauri::command]
pub async fn add_custom_trigger(
    app: tauri::AppHandle,
    plugin_id: String,
    keyword: String,
) -> Result<(), String> {
    let engine = app.state::<std::sync::Arc<crate::domain::plugin::PluginEngine>>();
    let router = app.state::<std::sync::Arc<crate::domain::intent::RuleRouter>>();

    let mut config = engine.get_config(&plugin_id).unwrap_or_default();

    // 检查是否已存在（不区分大小写，简单重复检查）
    let keyword_lower = keyword.to_lowercase();
    if config.custom_triggers.iter().any(|t| t.keyword.to_lowercase() == keyword_lower) {
        return Err(format!("触发词 '{keyword}' 已存在"));
    }

    // 添加新触发词
    config.custom_triggers.push(crate::app::config::CustomTrigger {
        keyword: keyword.clone(),
        enabled: true,
        surface: None,
    });

    engine.update_config(&plugin_id, config, Some(&router)).await?;
    tracing::info!(plugin_id, keyword, "自定义触发词已添加");
    Ok(())
}

/// 删除一个自定义触发词。
#[tauri::command]
pub async fn delete_custom_trigger(
    app: tauri::AppHandle,
    plugin_id: String,
    keyword: String,
) -> Result<(), String> {
    let engine = app.state::<std::sync::Arc<crate::domain::plugin::PluginEngine>>();
    let router = app.state::<std::sync::Arc<crate::domain::intent::RuleRouter>>();

    let mut config = engine.get_config(&plugin_id).unwrap_or_default();
    let before_len = config.custom_triggers.len();
    config.custom_triggers.retain(|t| t.keyword != keyword);

    if config.custom_triggers.len() == before_len {
        return Err(format!("触发词 '{keyword}' 不存在"));
    }

    engine.update_config(&plugin_id, config, Some(&router)).await?;
    tracing::info!(plugin_id, keyword, "自定义触发词已删除");
    Ok(())
}

/// 获取引擎配置（通用 API）。
#[tauri::command]
pub async fn get_engine_config(
    app: tauri::AppHandle,
    engine_id: String,
) -> Result<serde_json::Value, String> {
    let pool = app.state::<sqlx::SqlitePool>();
    Ok(crate::app::config::get_engine_config(&pool, &engine_id)
        .await
        .unwrap_or_else(|| serde_json::json!({})))
}

/// 获取 Context 层配置（设置页用）。优先读内存 state（最新），兜底读 DB。
#[tauri::command]
pub async fn get_context_config(
    app: tauri::AppHandle,
) -> Result<crate::app::config::ContextConfig, String> {
    if let Some(mem) =
        app.try_state::<std::sync::Arc<std::sync::RwLock<crate::app::config::ContextConfig>>>()
    {
        return Ok(mem.read().unwrap().clone());
    }
    let pool = app.state::<sqlx::SqlitePool>();
    Ok(crate::app::config::get_context_config(&pool).await)
}

/// 打开文件/快捷方式所在文件夹（explorer /select 定位选中）。
/// §5 约束：lnk_path 不归一化，透传原路径字符串。
/// 但 explorer /select 对正斜杠路径解析异常（会打开"文档"等默认位置），需归一化为反斜杠。
#[tauri::command]
pub async fn open_containing_folder(path: String) -> Result<(), String> {
    let path = path.trim();
    if path.is_empty() {
        return Err("路径为空".into());
    }
    // explorer /select 不认正斜杠，统一为反斜杠
    let normalized = path.replace('/', "\\");
    tracing::info!(original = %path, normalized = %normalized, "open_containing_folder");

    // 用 ShellExecuteW 直接调 explorer——绕过 std::process::Command 的参数拼接，
    // 避免 CreateProcessW 对含空格/特殊字符路径的转义问题。
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::{PCWSTR, w};

    let arg = format!("/select,{normalized}");
    let arg_wide: Vec<u16> = arg.encode_utf16().chain(std::iter::once(0)).collect();
    let result = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            w!("explorer"),
            PCWSTR(arg_wide.as_ptr()),
            None,
            SW_SHOWNORMAL,
        )
    };
    // ShellExecuteW 返回值 > 32 表示成功
    if result.0 as i32 <= 32 {
        return Err(format!("ShellExecuteW 失败，返回值: {}", result.0 as i32));
    }
    Ok(())
}

/// 解析 .lnk 快捷方式目标，用 explorer /select 定位到目标文件。
/// 非文件路径的快捷方式（URL、UWP 等）会返回错误。
#[tauri::command]
pub async fn open_lnk_target(lnk_path: String) -> Result<(), String> {
    tokio::task::spawn_blocking(move || {
        use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoUninitialize, IPersistFile, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
        use windows::Win32::Storage::FileSystem::WIN32_FIND_DATAW;
        use windows::Win32::UI::Shell::{IShellLinkW, ShellExecuteW};
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        use windows::core::{Interface, GUID, PCWSTR, w};

        // CLSID_ShellLink（00021401-0000-0000-C000-000000000046）
        const CLSID_SHELLLINK: GUID = GUID::from_u128(0x00021401_0000_0000_C000_000000000046);

        // COM 初始化（与 icon.rs 同模式：APARTMENTTHREADED，已初始化则跳过）
        let com_hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        let should_uninit = com_hr.is_ok();
        struct ComUninit(bool);
        impl Drop for ComUninit {
            fn drop(&mut self) { if self.0 { unsafe { CoUninitialize() }; } }
        }
        let _com = ComUninit(should_uninit);

        unsafe {
            // 创建 ShellLink COM 对象
            let link: IShellLinkW = CoCreateInstance(
                &CLSID_SHELLLINK,
                None,
                CLSCTX_INPROC_SERVER,
            ).map_err(|e| format!("创建 ShellLink 失败: {e}"))?;

            // 加载 .lnk 文件
            let persist: IPersistFile = link.cast()
                .map_err(|e| format!("获取 IPersistFile 失败: {e}"))?;
            let lnk_wide: Vec<u16> = lnk_path.encode_utf16().chain(std::iter::once(0)).collect();
            persist.Load(PCWSTR(lnk_wide.as_ptr()), windows::Win32::System::Com::STGM_READ)
                .map_err(|e| format!("加载 .lnk 失败: {e}"))?;

            // 解析目标路径
            let mut buf = [0u16; 1024];
            let mut find_data: WIN32_FIND_DATAW = std::mem::zeroed();
            link.GetPath(&mut buf, &mut find_data as *mut _, 0)
                .map_err(|e| format!("获取目标路径失败: {e}"))?;

            let target = PCWSTR(buf.as_ptr()).to_string()
                .map_err(|e| format!("路径转换失败: {e}"))?;
            let target = target.trim();

            if target.is_empty() {
                return Err("快捷方式未指向文件路径（可能是 URL 或 UWP 应用）".into());
            }

            // 用 explorer /select 定位到目标文件
            let normalized = target.replace('/', "\\");
            let arg = format!("/select,{normalized}");
            let arg_wide: Vec<u16> = arg.encode_utf16().chain(std::iter::once(0)).collect();
            let result = ShellExecuteW(
                None,
                w!("open"),
                w!("explorer"),
                PCWSTR(arg_wide.as_ptr()),
                None,
                SW_SHOWNORMAL,
            );
            if result.0 as i32 <= 32 {
                return Err(format!("ShellExecuteW 失败，返回值: {}", result.0 as i32));
            }
        }
        Ok(())
    })
    .await
    .map_err(|e| format!("spawn_blocking 失败: {e}"))?
}

/// 将文本写入系统剪贴板（Windows API）。
/// 右键菜单独立 Popup 窗口中 navigator.clipboard 不可靠，改走后端。
#[tauri::command]
pub async fn copy_to_clipboard(text: String) -> Result<(), String> {
    tokio::task::spawn_blocking(move || {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
        use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};

        // RAII guard: 确保 CloseClipboard 在所有路径上被调用
        struct ClipboardGuard;
        impl Drop for ClipboardGuard {
            fn drop(&mut self) { unsafe { let _ = CloseClipboard(); } }
        }

        unsafe {
            if OpenClipboard(Some(HWND(std::ptr::null_mut()))).is_err() {
                return Err("打开剪贴板失败".into());
            }
            let _guard = ClipboardGuard;

            let _ = EmptyClipboard();

            // 分配全局内存（+1 for null terminator）
            let wchars: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
            let byte_size = wchars.len() * 2;
            let hmem = GlobalAlloc(GMEM_MOVEABLE, byte_size)
                .map_err(|e| format!("GlobalAlloc 失败: {e}"))?;
            let ptr = GlobalLock(hmem) as *mut u16;
            if ptr.is_null() {
                return Err("GlobalLock 失败".into());
            }
            std::ptr::copy_nonoverlapping(wchars.as_ptr(), ptr, wchars.len());
            let _ = GlobalUnlock(hmem);

            // CF_UNICODETEXT = 13; SetClipboardData 要求 HANDLE 而非 HGLOBAL
            if SetClipboardData(13, Some(std::mem::transmute(hmem))).is_err() {
                return Err("SetClipboardData 失败".into());
            }
        }
        Ok(())
    })
    .await
    .map_err(|e| format!("spawn_blocking 失败: {e}"))?
}

/// 重置某项的历史记录权重（右键菜单「重置该项记录」，0.5.3）。
#[tauri::command]
pub async fn reset_item_history(app: tauri::AppHandle, lnk_path: String) -> Result<(), String> {
    let pool = app.state::<sqlx::SqlitePool>();
    crate::infra::data::history::reset_weight(&pool, &lnk_path).await;
    tracing::debug!(path = %lnk_path, "已重置该项历史权重");
    Ok(())
}

/// 列出当前有可见窗口的运行中进程（设置页「敏感应用」选择器用）。
/// spawn_blocking 隔离 Win32 枚举，避免阻塞 async runtime。
#[tauri::command]
pub async fn list_running_processes() -> Vec<crate::infra::platform::context::RunningProcess> {
    tokio::task::spawn_blocking(crate::infra::platform::context::list_running_processes)
        .await
        .unwrap_or_default()
}

/// 录制快捷键（阻塞，直到用户按下组合键或超时）。
#[tauri::command]
pub async fn record_hotkey() -> Result<serde_json::Value, String> {
    // 在阻塞线程中等待录制（事件由 ll_proc 喂入 recorder 状态机）
    let result = tokio::task::spawn_blocking(|| {
        crate::infra::platform::hotkey::record_hotkey_blocking()
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

// ── 右键菜单独立窗口（0.5.3+） ───────────────────────────────────────────────

/// 显示右键菜单独立窗口（突破主窗口边界裁剪）。
/// 复用已有窗口：首次创建，后续 hide → 更新数据 → show，避免重复创建 WebView2 的开销。
#[tauri::command]
pub async fn show_context_menu(
    app: tauri::AppHandle,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    items: String,
) -> Result<(), String> {
    use tauri::{WebviewUrl, WebviewWindowBuilder};

    // 主题 resolve（auto → dark/light）
    let theme = {
        let pool = app.state::<sqlx::SqlitePool>();
        let raw = crate::app::config::get_config(&pool).await.theme;
        if raw == "auto" {
            let is_light = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
                .open_subkey("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize")
                .and_then(|k| k.get_value::<u32, _>("AppsUseLightTheme"))
                .map(|v| v == 1)
                .unwrap_or(false);
            if is_light { "light".to_string() } else { "dark".to_string() }
        } else {
            raw
        }
    };

    // 复用已有窗口：resize → reposition → show → eval 渲染新数据 → force_topmost
    // ⚠️ 不能在隐藏态用 emit 传数据：WebView2 在 IsVisible=false 时会丢弃事件
    // （曾导致「窗口尺寸已撑开、内容却没更新」）。改用 eval（走 ExecuteScript 注入
    // 脚本到 webview 队列，show 之后必执行），比事件系统更可靠地更新菜单内容。
    if let Some(win) = app.get_webview_window("context-menu") {
        let _ = win.set_size(tauri::PhysicalSize::new(width as u32, height as u32));
        let _ = win.set_position(tauri::PhysicalPosition::new(x as i32, y as i32));
        let _ = win.show();
        let theme_js = serde_json::to_string(&theme).unwrap_or_else(|_| "\"dark\"".to_string());
        let js = format!(
            "window.__renderContextMenu && window.__renderContextMenu({items}, {theme})",
            items = items,
            theme = theme_js,
        );
        let _ = win.eval(&js);
        // Win32 直接设 TOPMOST，比 Tauri 的 set_always_on_top 更可靠
        if let Ok(hwnd) = win.hwnd() {
            crate::infra::platform::window::force_topmost(windows::Win32::Foundation::HWND(hwnd.0 as _));
        }
        tracing::trace!(x, y, width, height, items_len = items.len(), "右键菜单窗口复用");
        return Ok(());
    }

    // 首次创建：通过 URL 参数传递初始数据
    let encoded_items = urlencoding::encode(&items).to_string();
    let url = format!("contextmenu-popup.html?items={encoded_items}&theme={theme}");
    tracing::debug!(x, y, width, height, "创建右键菜单窗口");
    let _win = WebviewWindowBuilder::new(
        &app,
        "context-menu",
        WebviewUrl::App(url.into()),
    )
    .title("")
    .inner_size(width, height)
    .position(x, y)
    .decorations(false)
    .transparent(false)
    .always_on_top(true)
    .skip_taskbar(true)
    .visible(true)
    .focused(false)
    .resizable(false)
    .build()
    .map_err(|e| format!("创建右键菜单窗口失败: {e}"))?;

    // 首次创建也走 Win32 强制置顶（与复用路径一致）
    if let Ok(hwnd) = _win.hwnd() {
        crate::infra::platform::window::force_topmost(windows::Win32::Foundation::HWND(hwnd.0 as _));
    }

    tracing::trace!(x, y, width, height, items_len = items.len(), "右键菜单窗口已创建");
    Ok(())
}

/// 隐藏右键菜单窗口（hide 而非 close，保留窗口供下次复用）。
#[tauri::command]
pub async fn hide_context_menu(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(win) = app.get_webview_window("context-menu") {
        let _ = win.hide();
        tracing::trace!("hide_context_menu: 已隐藏右键菜单窗口");
    }
    Ok(())
}

/// Popup 窗口菜单项被点击 → 通知主窗口执行动作。
/// action_id 是菜单项的唯一标识（JSON 数组索引）。
///
/// **顺序很重要**：先隐藏 Popup + 主窗口获焦，再 emit 事件。
/// 否则前端收到事件时 Popup 仍是前台窗口，`document.hasFocus() === false`，
/// `navigator.clipboard.readText()` 会被 Chromium 以「document 未获焦」为由拒绝，
/// `execCommand("paste")` 同样失效——症状就是「点粘贴，输入框仍空」（右键在
/// 主窗口边框时尤其容易复现，此时主窗口本就不是前台）。
#[tauri::command]
pub async fn context_menu_action(app: tauri::AppHandle, action_id: u32) -> Result<(), String> {
    // 1. 先隐藏 Popup 窗口，让主窗口有机会重回前台
    hide_context_menu(app.clone()).await?;
    // 2. 显式把主窗口置为前台并聚焦，保证 clipboard/execCommand 可用
    if let Some(main) = app.get_webview_window("main") {
        let _ = main.show();
        let _ = main.set_focus();
    }
    // 3. 最后再通知前端执行动作
    app.emit("blink://context-menu-action", action_id)
        .map_err(|e| e.to_string())?;
    Ok(())
}

// ─── 脚本解释器配置（Phase 0.6） ───────────────────────────────────────────

/// 探测系统中可用的脚本解释器状态。
#[tauri::command]
pub async fn probe_interpreters() -> crate::domain::plugin::InterpretersStatus {
    tracing::debug!("探测脚本解释器状态");
    crate::domain::plugin::probe_interpreters()
}

// ─── 剪贴板历史（Phase 0.7.3）──────────────────────────────────────────────────

/// 获取最近的剪贴板历史。
#[tauri::command]
pub async fn get_clipboard_history(
    app: tauri::AppHandle,
    limit: Option<i64>,
) -> Vec<crate::infra::data::clipboard::ClipboardItem> {
    let pool = app.state::<sqlx::SqlitePool>();
    crate::infra::data::clipboard::query_recent(&pool, limit.unwrap_or(20)).await
}

/// 搜索剪贴板历史。
#[tauri::command]
pub async fn search_clipboard_history(
    app: tauri::AppHandle,
    query: String,
    limit: Option<i64>,
) -> Vec<crate::infra::data::clipboard::ClipboardItem> {
    let pool = app.state::<sqlx::SqlitePool>();
    crate::infra::data::clipboard::search(&pool, &query, limit.unwrap_or(20)).await
}

/// 记录剪贴板命中（用户选择粘贴某条历史）。
#[tauri::command]
pub async fn record_clipboard_hit(app: tauri::AppHandle, id: String) -> Result<(), String> {
    let pool = app.state::<sqlx::SqlitePool>();
    crate::infra::data::clipboard::record_hit(&pool, &id).await;
    Ok(())
}

/// 删除指定剪贴板条目。
#[tauri::command]
pub async fn delete_clipboard_item(app: tauri::AppHandle, id: String) -> Result<(), String> {
    let pool = app.state::<sqlx::SqlitePool>();
    crate::infra::data::clipboard::delete_item(&pool, &id).await;
    Ok(())
}

/// 清空所有剪贴板历史。
#[tauri::command]
pub async fn clear_clipboard_history(app: tauri::AppHandle) -> Result<(), String> {
    let pool = app.state::<sqlx::SqlitePool>();
    crate::infra::data::clipboard::clear_all(&pool).await;
    Ok(())
}

/// 获取剪贴板统计信息。
#[tauri::command]
pub async fn get_clipboard_stats(app: tauri::AppHandle) -> serde_json::Value {
    let pool = app.state::<sqlx::SqlitePool>();
    crate::infra::data::clipboard::get_stats(&pool).await
}

// ─── 性能统计（Phase 0.7.0）──────────────────────────────────────────────────

/// 获取性能统计概览（设置页 → 调试 Tab）。
#[tauri::command]
pub async fn get_perf_overview(app: tauri::AppHandle) -> serde_json::Value {
    let pool = app.state::<sqlx::SqlitePool>();
    crate::infra::utils::perf::get_overview(&pool).await
}

/// 查询指定指标的 P50/P90/P99。
#[tauri::command]
pub async fn get_perf_percentiles(
    app: tauri::AppHandle,
    category: String,
    name: String,
    limit: Option<i64>,
) -> serde_json::Value {
    let pool = app.state::<sqlx::SqlitePool>();
    crate::infra::utils::perf::query_percentiles(&pool, &category, &name, limit.unwrap_or(100)).await
}

/// 查询慢查询日志。
#[tauri::command]
pub async fn get_perf_slow_queries(
    app: tauri::AppHandle,
    category: String,
    threshold_ms: f64,
    limit: Option<i64>,
) -> Vec<crate::infra::utils::perf::PerformanceMetric> {
    let pool = app.state::<sqlx::SqlitePool>();
    crate::infra::utils::perf::query_slow(&pool, &category, threshold_ms, limit.unwrap_or(20)).await
}

/// 查询最近 N 条性能指标。
#[tauri::command]
pub async fn get_perf_recent(
    app: tauri::AppHandle,
    limit: Option<i64>,
) -> Vec<crate::infra::utils::perf::PerformanceMetric> {
    let pool = app.state::<sqlx::SqlitePool>();
    crate::infra::utils::perf::query_recent(&pool, limit.unwrap_or(100)).await
}

/// 导出性能报告（JSON 格式）。
/// 弹出保存文件对话框，用户选择路径后写入文件，返回保存的路径（取消时返回 null）。
#[tauri::command]
pub async fn export_perf_report(app: tauri::AppHandle) -> Result<Option<String>, String> {
    let pool = app.state::<sqlx::SqlitePool>();
    let report = crate::infra::utils::perf::export_report(&pool).await;

    // 弹出保存文件对话框
    let default_name = format!(
        "blink-perf-report-{}.json",
        chrono::Local::now().format("%Y-%m-%d")
    );

    let file_path = app
        .dialog()
        .file()
        .set_title("导出性能报告")
        .add_filter("JSON 文件", &["json"])
        .set_file_name(&default_name)
        .blocking_save_file()
        .and_then(|p| match p {
            tauri_plugin_dialog::FilePath::Path(path) => path.to_str().map(|s| s.to_string()),
            tauri_plugin_dialog::FilePath::Url(url) => Some(url.to_string()),
        });

    let Some(path) = file_path else {
        return Ok(None); // 用户取消了
    };

    // 写入文件
    let json_str = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
    tokio::fs::write(&path, json_str)
        .await
        .map_err(|e| e.to_string())?;

    tracing::info!(path = %path, "性能报告已导出");
    Ok(Some(path))
}

/// 清除全部性能指标数据。
#[tauri::command]
pub async fn clear_perf_data(app: tauri::AppHandle) -> Result<u64, String> {
    let pool = app.state::<sqlx::SqlitePool>();
    crate::infra::utils::perf::clear_all(&pool).await
}

/// 在外部浏览器打开 URL。
#[tauri::command]
pub async fn open_url(url: String) -> Result<(), String> {
    tracing::debug!(%url, "open_url");

    // 使用 Windows ShellExecuteW 打开默认浏览器
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::UI::Shell::ShellExecuteW;
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        use windows::core::{PCWSTR, w};

        let url_wide: Vec<u16> = url.encode_utf16().chain(std::iter::once(0)).collect();
        let result = unsafe {
            ShellExecuteW(
                None,
                w!("open"),
                PCWSTR(url_wide.as_ptr()),
                None,
                None,
                SW_SHOWNORMAL,
            )
        };
        if result.0 as i32 <= 32 {
            return Err(format!("打开 URL 失败，返回值: {}", result.0 as i32));
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        // 非 Windows 平台使用 open crate（后续可添加）
        return Err("当前平台暂不支持打开 URL".to_string());
    }

    Ok(())
}

// ── 泛型配置命令（0.8.6 §8.1.3 ConfigStore）──────────────────────────────────

/// 泛型配置读取（0.8.6 §8.1.3）。
///
/// 前端 `invoke("get_config_section", { key: "app_config" })` → 返回该 key 的 JSON 值。
/// 不存在返回 `null`（前端自行 fallback 到默认值）。
///
/// **key 命名空间**：
/// - `app_config`：完整 AppConfig（兼容旧 key）
/// - `engine:{id}`：引擎配置（start_menu / calc / file_search）
/// - `plugin:{id}`：插件配置
/// - `context:config`：Context 层配置
///
/// 0.9 扩展：`ai.provider` / `ai.chat` 等直接加 key，零脚手架。
#[tauri::command]
pub async fn get_config_section(
    app: tauri::AppHandle,
    key: String,
) -> Result<serde_json::Value, String> {
    let pool = app.state::<sqlx::SqlitePool>();
    let json_str = crate::infra::data::history::get_config(&pool, &key).await;
    match json_str {
        Some(s) => serde_json::from_str(&s).map_err(|e| format!("配置解析失败: {e}")),
        None => Ok(serde_json::Value::Null),
    }
}

/// 泛型配置写入（0.8.6 §8.1.3）。
///
/// 前端 `invoke("set_config_section", { key: "app_config", value: {...} })` → 写入 SQLite。
/// 写入成功后 emit `blink://config-changed` 事件，前端各模块按需订阅。
///
/// **幂等性**：直接覆盖写，不需要先读后写。
#[tauri::command]
pub async fn set_config_section(
    app: tauri::AppHandle,
    key: String,
    value: serde_json::Value,
) -> Result<(), String> {
    let pool = app.state::<sqlx::SqlitePool>();
    let json = serde_json::to_string(&value).map_err(|e| format!("序列化失败: {e}"))?;
    crate::infra::data::history::set_config(&pool, &key, &json).await.map_err(|e| format!("配置写入失败: {e}"))?;

    // 广播配置变更事件（前端各模块按 key 订阅）
    if let Err(e) = app.emit("blink://config-changed", serde_json::json!({ "key": key })) {
        tracing::debug!(error = %e, "emit blink://config-changed failed");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::version_gt;

    #[test]
    fn version_gt_basic() {
        assert!(version_gt("0.9.0", "0.8.8"));
        assert!(version_gt("1.0.0", "0.99.99"));
        assert!(!version_gt("0.8.8", "0.8.8"));
        assert!(!version_gt("0.8.7", "0.8.8"));
    }

    #[test]
    fn version_gt_patch() {
        assert!(version_gt("0.8.9", "0.8.8"));
        assert!(!version_gt("0.8.8", "0.8.9"));
    }

    #[test]
    fn version_gt_malformed() {
        // 不完整版本号也能比较（缺失部分按 0 算）
        assert!(version_gt("0.9", "0.8.8"));
        assert!(!version_gt("0.8", "0.8.1"));
    }
}
