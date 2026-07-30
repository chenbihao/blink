//! diagnostic 域命令（0.14.6 §2.4 从 commands.rs 拆分）。

use tauri::Manager;
use tauri_plugin_dialog::DialogExt;

/// **临时**（0.11.7-f 调试用）：前端把 console 日志转发到后端 tracing。
///
/// TODO(0.11.7 收尾)：0.11.7 稳定后删除此 command 与前端 `frontendLog()` 封装。
/// 前端诊断转由 devtools 完成。
#[tauri::command]
pub fn frontend_log(level: String, message: String) {
    match level.as_str() {
        "error" => tracing::error!(target: "blink::frontend", "{message}"),
        "warn" => tracing::warn!(target: "blink::frontend", "{message}"),
        "info" => tracing::info!(target: "blink::frontend", "{message}"),
        "debug" => tracing::debug!(target: "blink::frontend", "{message}"),
        _ => tracing::trace!(target: "blink::frontend", "{message}"),
    }
}

/// 设置页-存储：获取四库统计信息（0.12.0 DB 四层拆分）。
///
/// 返回各库的行数 + 文件大小 + 路径，前端渲染分区展示。
#[tauri::command]
pub async fn get_storage_info(app: tauri::AppHandle) -> serde_json::Value {
    let pools = app.state::<crate::infra::data::DbPools>();

    // 历史库：history + clipboard_history 行数
    let history_count = crate::infra::data::history::count(&pools.history).await;
    let clipboard_stats = crate::infra::data::clipboard::get_stats(&pools.history).await;
    let clipboard_count = clipboard_stats
        .get("total")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);

    // AI 库：ai_tool_audit 行数
    let ai_audit_count = crate::infra::data::ai_audit::count(&pools.ai).await;

    // 缓存库：performance_metrics + icon_cache 行数
    // 新代码直接用 data 层真源（utils::perf 的 count 是 re-export，仅兼容旧调用点）
    let perf_count = crate::infra::data::perf::count(&pools.cache).await;
    let icon_cache_count = crate::infra::data::icon_cache::count(&pools.cache).await;

    // 文件大小
    let data_dir = crate::infra::utils::paths::app_data_dir();

    // P2.7: 迁移失败标记（若有，前端存储面板显示警告）
    // 优化：如果旧 blink.db 已不存在，说明迁移已成功完成，migration_failed 是残留标记 → 清除
    let legacy_db_path = data_dir.join("blink.db");
    let legacy_db_exists = legacy_db_path.exists();
    let mut migration_failed: Option<String> =
        sqlx::query_scalar("SELECT value FROM config WHERE key = 'migration_failed'")
            .fetch_optional(&pools.config)
            .await
            .ok()
            .flatten();
    tracing::info!(
        legacy_db_exists,
        migration_failed_set = migration_failed.is_some(),
        "get_storage_info: 迁移标记检查"
    );
    if migration_failed.is_some() && !legacy_db_exists {
        tracing::info!("旧 blink.db 已删除但 migration_failed 标记仍在，清除残留标记");
        let _ = sqlx::query("DELETE FROM config WHERE key = 'migration_failed'")
            .execute(&pools.config)
            .await;
        migration_failed = None;
    }

    serde_json::json!({
        "databases": {
            "config": {
                "name": "配置库",
                "file": "blink_config.db",
                "size_bytes": file_size(&data_dir.join("blink_config.db")),
                "path": data_dir.join("blink_config.db").display().to_string(),
            },
            "history": {
                "name": "历史库",
                "file": "blink_history.db",
                "size_bytes": file_size(&data_dir.join("blink_history.db")),
                "path": data_dir.join("blink_history.db").display().to_string(),
                "history_count": history_count,
                "clipboard_count": clipboard_count,
            },
            "ai": {
                "name": "AI 库",
                "file": "blink_ai.db",
                "size_bytes": file_size(&data_dir.join("blink_ai.db")),
                "path": data_dir.join("blink_ai.db").display().to_string(),
                "audit_count": ai_audit_count,
            },
            "cache": {
                "name": "缓存库",
                "file": "blink_cache.db",
                "size_bytes": file_size(&data_dir.join("blink_cache.db")),
                "path": data_dir.join("blink_cache.db").display().to_string(),
                "perf_count": perf_count,
                "icon_cache_count": icon_cache_count,
            },
        },
        "data_dir": data_dir.display().to_string(),
        // P2.7: 迁移失败标记（None = 正常；Some(reason) = 旧库迁移失败，前端显示警告）
        "migration_failed": migration_failed,
        // 兼容旧前端字段
        "history_count": history_count,
        "db_path": data_dir.display().to_string(),
    })
}

/// 设置页-存储：打开数据文件夹（0.12.0 §2.2.7）。
///
/// 调 `ShellExecuteW("explorer", %APPDATA%\blink)` 打开数据目录。
#[tauri::command]
pub fn open_data_folder() -> Result<(), String> {
    let data_dir = crate::infra::utils::paths::app_data_dir();
    // 目录不存在时先创建，避免 explorer 打开"文档"等默认位置
    if !data_dir.exists() {
        if let Err(e) = std::fs::create_dir_all(&data_dir) {
            return Err(format!("创建数据目录失败: {e}"));
        }
    }
    std::process::Command::new("explorer.exe")
        .arg(&data_dir)
        .spawn()
        .map_err(|e| format!("打开文件夹失败: {e}"))?;
    Ok(())
}

/// 清除 `migration_failed` 标记。
///
/// 用户点击"重试迁移"按钮时调用：
/// - 若旧 blink.db 已不存在（迁移已成功）→ 仅清除残留标记，无需重启
/// - 若旧 blink.db 仍存在但 db_split_done=true（迁移成功但旧库未删）→ 删除旧库 + 清标记
/// - 若旧 blink.db 仍存在且 db_split_done 未设 → 清标记，需重启重试迁移
#[tauri::command]
pub async fn retry_migration(app: tauri::AppHandle) -> Result<(), String> {
    let pools = app.state::<crate::infra::data::DbPools>();

    // 清除 migration_failed 标记
    sqlx::query("DELETE FROM config WHERE key = 'migration_failed'")
        .execute(&pools.config)
        .await
        .map_err(|e| format!("清除迁移标记失败: {e}"))?;
    tracing::info!("已清除 migration_failed 标记");

    // 检查旧库是否仍存在
    let data_dir = crate::infra::utils::paths::app_data_dir();
    let legacy_path = data_dir.join("blink.db");

    if legacy_path.exists() {
        // 检查 db_split_done 是否已设
        let db_split_done: Option<String> =
            sqlx::query_scalar("SELECT value FROM config WHERE key = 'db_split_done'")
                .fetch_optional(&pools.config)
                .await
                .ok()
                .flatten();

        if db_split_done.as_deref() == Some("true") {
            // 迁移已成功，旧库是残留 → 直接删除
            match std::fs::remove_file(&legacy_path) {
                Ok(()) => tracing::info!("retry_migration: 旧 blink.db 已删除"),
                Err(e) => tracing::warn!(error = %e, "retry_migration: 删除旧 blink.db 失败"),
            }
        } else {
            tracing::info!("retry_migration: 旧 blink.db 仍存在且迁移未完成，需重启重试");
        }
    } else {
        tracing::info!("retry_migration: 旧 blink.db 不存在，标记已清除");
    }

    Ok(())
}

/// 设置页-存储：清空缓存库（0.12.0 §2.2.7）。
///
/// 清空 performance_metrics + icon_cache 两表。缓存可重建，清空无风险。
#[tauri::command]
pub async fn clear_cache_db(app: tauri::AppHandle) -> Result<(), String> {
    let pools = app.state::<crate::infra::data::DbPools>();
    // 清空 performance_metrics
    crate::infra::data::perf::clear_all(&pools.cache)
        .await
        .map_err(|e| format!("清空 performance_metrics 失败: {e}"))?;
    // 清空 icon_cache
    crate::infra::data::icon_cache::clear_all(&pools.cache).await;
    tracing::info!("缓存库已清空（performance_metrics + icon_cache）");
    Ok(())
}

/// 设置页-关于：应用元信息（版本/名称/描述/仓库）。
/// 版本从 Cargo.toml 编译期注入（`CARGO_PKG_*`），tauri.conf.json 版本单独在 bundle 层使用。
/// CI release workflow 会根据 git tag 自动同步两处版本；本地开发手动维护 Cargo.toml 即可。
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
///
/// 流程：请求 GitHub API `/repos/{owner/repo}/releases/latest` →
/// 取 `tag_name` 去掉 `v` 前缀 → semver 比较与当前版本。
///
/// 返回 JSON：
/// - 成功：`{ has_update, current_version, latest_version, release_url }`
/// - 网络失败：`{ has_update: false, current_version, error: "..." }`
///
/// **走全局代理**：如果用户配置了 `engine:_global_proxy`，检查更新请求也走代理。
/// 国内直连 `api.github.com` 极易超时，这是此前「检查更新无效」的根因。
#[tauri::command]
pub async fn check_update(app: tauri::AppHandle) -> serde_json::Value {
    let current = env!("CARGO_PKG_VERSION");
    let repo = env!("CARGO_PKG_REPOSITORY");
    // 从 "https://github.com/owner/repo" 提取 "owner/repo"
    let repo_path = repo
        .trim_start_matches("https://github.com/")
        .trim_start_matches("http://github.com/")
        .trim_end_matches('/');

    let api_url = format!("https://api.github.com/repos/{repo_path}/releases/latest");

    // 读取全局代理配置，与插件 HTTP 请求共用
    let proxy_url = {
        let pool = &app.state::<crate::infra::data::DbPools>().config;
        let cfg = crate::app::config::get_engine_config(&pool, "_global_proxy").await;
        cfg.and_then(|v| {
            let https = v
                .get("https")
                .and_then(|s| s.as_str())
                .filter(|s| !s.is_empty());
            let http = v
                .get("http")
                .and_then(|s| s.as_str())
                .filter(|s| !s.is_empty());
            https.or(http).map(|s| s.to_string())
        })
    };

    let mut builder = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent("blink-updater");

    if let Some(ref url) = proxy_url {
        match reqwest::Proxy::all(url) {
            Ok(proxy) => builder = builder.proxy(proxy),
            Err(e) => tracing::warn!(%e, proxy = %url, "check_update: 代理配置无效，回退直连"),
        }
    }

    let client = builder.build().unwrap_or_default();

    let resp_result = client.get(&api_url).send().await;
    let resp = match resp_result {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(%e, "check_update: 请求 GitHub API 失败");
            return serde_json::json!({
                "has_update": false,
                "current_version": current,
                "error": format!("网络请求失败: {e}"),
            });
        }
    };
    if !resp.status().is_success() {
        tracing::warn!(status = %resp.status(), "check_update: GitHub API 返回非 2xx");
        return serde_json::json!({
            "has_update": false,
            "current_version": current,
            "error": format!("GitHub API 返回 {}", resp.status()),
        });
    }
    let body = match resp.json::<serde_json::Value>().await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(%e, "check_update: 解析 JSON 失败");
            return serde_json::json!({
                "has_update": false,
                "current_version": current,
                "error": "响应解析失败".to_string(),
            });
        }
    };

    let tag = body["tag_name"].as_str().unwrap_or("");
    let latest = tag.trim_start_matches('v');
    let release_url = body["html_url"]
        .as_str()
        .unwrap_or(&format!("https://github.com/{repo_path}/releases/latest"))
        .to_string();

    let has_update = version_gt(latest, current);
    if has_update {
        tracing::info!(current, latest, "发现新版本");
    } else {
        tracing::debug!(current, latest, "已是最新版本");
    }

    serde_json::json!({
        "has_update": has_update,
        "current_version": current,
        "latest_version": latest,
        "release_url": release_url,
    })
}

/// 打开当天日志文件（资源管理器中定位；文件不存在则打开文件夹）。
#[tauri::command]
pub fn open_log_file() -> Result<(), String> {
    let path = crate::infra::utils::logging::current_log_file();
    let arg = if path.exists() {
        format!("/select,{}", path.display())
    } else {
        // 当天尚无日志（如 error 级未产生），直接打开文件夹
        crate::infra::utils::logging::log_dir()
            .display()
            .to_string()
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

/// 探测系统中可用的脚本解释器状态。
///
/// 如果提供了 `python_path` 或 `node_path`，优先验证该路径（用户手动配置），
/// 无效时才回退到 PATH 扫描。
#[tauri::command]
pub async fn probe_interpreters(
    python_path: Option<String>,
    node_path: Option<String>,
) -> crate::domain::plugin::InterpretersStatus {
    tracing::debug!(?python_path, ?node_path, "探测脚本解释器状态");
    crate::domain::plugin::probe_interpreters(python_path.as_deref(), node_path.as_deref())
}

/// 获取已保存的解释器路径配置。
#[tauri::command]
pub async fn get_interpreter_paths(app: tauri::AppHandle) -> serde_json::Value {
    let pool = &app.state::<crate::infra::data::DbPools>().config;
    crate::infra::data::history::get_config(&pool, "interpreter_paths")
        .await
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(serde_json::json!({}))
}

/// 获取性能统计概览（设置页 → 调试 Tab）。
#[tauri::command]
pub async fn get_perf_overview(app: tauri::AppHandle) -> serde_json::Value {
    let pool = &app.state::<crate::infra::data::DbPools>().cache;
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
    let pool = &app.state::<crate::infra::data::DbPools>().cache;
    crate::infra::utils::perf::query_percentiles(&pool, &category, &name, limit.unwrap_or(100))
        .await
}

/// 查询慢查询日志。
#[tauri::command]
pub async fn get_perf_slow_queries(
    app: tauri::AppHandle,
    category: String,
    threshold_ms: f64,
    limit: Option<i64>,
) -> Vec<crate::infra::utils::perf::PerformanceMetric> {
    let pool = &app.state::<crate::infra::data::DbPools>().cache;
    crate::infra::utils::perf::query_slow(&pool, &category, threshold_ms, limit.unwrap_or(20)).await
}

/// 查询最近 N 条性能指标。
#[tauri::command]
pub async fn get_perf_recent(
    app: tauri::AppHandle,
    limit: Option<i64>,
) -> Vec<crate::infra::utils::perf::PerformanceMetric> {
    let pool = &app.state::<crate::infra::data::DbPools>().cache;
    crate::infra::utils::perf::query_recent(&pool, limit.unwrap_or(100)).await
}

/// 导出性能报告（JSON 格式）。
/// 弹出保存文件对话框，用户选择路径后写入文件，返回保存的路径（取消时返回 null）。
#[tauri::command]
pub async fn export_perf_report(app: tauri::AppHandle) -> Result<Option<String>, String> {
    let pool = &app.state::<crate::infra::data::DbPools>().cache;
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
    let pool = &app.state::<crate::infra::data::DbPools>().cache;
    crate::infra::data::perf::clear_all(&pool).await
}

/// 0.13.6: CLI 能力识别——从 `--help` 输出生成 SKILL.md 模板。
///
/// 纯文本解析，零 LLM 依赖。生成的模板供用户 review 编辑。
/// 识别后保存到 `%APPDATA%\blink\skills\<tool-name>\SKILL.md`。
#[tauri::command]
pub async fn recognize_cli_tool(
    cli_path: String,
) -> Result<crate::domain::ai::cli_recognizer::CliRecognitionResult, String> {
    crate::domain::ai::cli_recognizer::recognize_cli(&cli_path).await
}

/// 获取文件大小（字节），不存在返回 0。
fn file_size(path: &std::path::Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// 语义化版本比较：a > b 则返回 true。
///
/// 优先用 `semver` 库严格比较（支持 pre-release / build metadata），
/// 解析失败时 fallback 到简单数字比较（兼容非标准版本号如 `0.9`）。
fn version_gt(a: &str, b: &str) -> bool {
    match (semver::Version::parse(a), semver::Version::parse(b)) {
        (Ok(va), Ok(vb)) => va > vb,
        _ => version_gt_fallback(a, b),
    }
}

/// Fallback：非标准版本号的简单数字比较，取前三段，缺失按 0 算。
fn version_gt_fallback(a: &str, b: &str) -> bool {
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

/// 递归计算目录大小（字节）。
pub(crate) fn dir_size_bytes(path: &std::path::Path) -> u64 {
    let mut total = 0u64;
    for entry in walkdir::WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if entry.file_type().is_file() {
            total += entry.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }
    total
}

/// 字节 → MB（保留两位小数）。
pub(crate) fn bytes_to_mb(bytes: u64) -> f64 {
    ((bytes as f64 / (1024.0 * 1024.0)) * 100.0).round() / 100.0
}
