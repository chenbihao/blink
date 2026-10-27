//! diagnostic 域命令（0.14.6 §2.4 从 commands.rs 拆分）。

use std::sync::atomic::{AtomicBool, Ordering};
use tauri::Manager;
use tauri::Emitter;
use tauri_plugin_dialog::DialogExt;

/// 设置页-存储：获取四库统计信息（0.12.0 DB 四层拆分）。
///
/// 返回各库的行数 + 文件大小 + 路径，前端渲染分区展示。
#[tauri::command]
pub async fn get_storage_info(app: tauri::AppHandle) -> serde_json::Value {
    let pools = app.state::<crate::infra::data::DbPools>();

    // 历史库：history + clipboard_history + clipboard_files 行数
    let history_count = crate::infra::data::history::count(&pools.history).await;
    let clipboard_stats = crate::infra::data::clipboard::get_stats(&pools.history).await;
    let clipboard_count = clipboard_stats
        .get("total")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    // 0.26.9：文件路径历史统计（条数 + 序列化载荷）
    let clipboard_file_stats =
        crate::infra::data::clipboard_files::get_file_stats(&pools.history).await;
    let clipboard_file_count = clipboard_file_stats["file_count"].as_i64().unwrap_or(0);
    let clipboard_file_payload_bytes = clipboard_file_stats["file_payload_bytes"]
        .as_i64()
        .unwrap_or(0);

    // AI 库：ai_tool_audit 行数 + conversations/messages 行数
    let ai_audit_count = crate::infra::data::ai_audit::count(&pools.ai).await;
    let ai_conversation_count =
        crate::infra::data::conversations::count_conversations(&pools.ai).await;
    let ai_message_count = crate::infra::data::conversations::count_messages(&pools.ai).await;

    // 缓存库：performance_metrics + icon_cache 行数
    // 新代码直接用 data 层真源（utils::perf 的 count 是 re-export，仅兼容旧调用点）
    let perf_count = crate::infra::data::perf::count(&pools.cache).await;
    let icon_cache_count = crate::infra::data::icon_cache::count(&pools.cache).await;

    // 0.17.0: 剪贴板图片统计
    let clipboard_image_count = crate::infra::data::clipboard_images::count(&pools.cache).await;
    let clipboard_image_stats =
        crate::infra::data::clipboard_images::get_image_stats(&pools.cache).await;
    let clipboard_image_size_bytes = clipboard_image_stats["total_size_bytes"]
        .as_i64()
        .unwrap_or(0);

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
                "size_bytes": db_size_bytes(&data_dir, "blink_config.db"),
                "path": data_dir.join("blink_config.db").display().to_string(),
            },
            "history": {
                "name": "历史库",
                "file": "blink_history.db",
                "size_bytes": db_size_bytes(&data_dir, "blink_history.db"),
                "path": data_dir.join("blink_history.db").display().to_string(),
                "history_count": history_count,
                "clipboard_count": clipboard_count,
                "clipboard_file_count": clipboard_file_count,
                "clipboard_file_payload_bytes": clipboard_file_payload_bytes,
            },
            "ai": {
                "name": "AI 库",
                "file": "blink_ai.db",
                "size_bytes": db_size_bytes(&data_dir, "blink_ai.db"),
                "path": data_dir.join("blink_ai.db").display().to_string(),
                "audit_count": ai_audit_count,
                "conversation_count": ai_conversation_count,
                "message_count": ai_message_count,
            },
            "cache": {
                "name": "缓存库",
                "file": "blink_cache.db",
                "size_bytes": db_size_bytes(&data_dir, "blink_cache.db"),
                "path": data_dir.join("blink_cache.db").display().to_string(),
                "perf_count": perf_count,
                "icon_cache_count": icon_cache_count,
                "clipboard_image_count": clipboard_image_count,
                "clipboard_image_size_bytes": clipboard_image_size_bytes,
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
    if !data_dir.exists()
        && let Err(e) = std::fs::create_dir_all(&data_dir)
    {
        return Err(format!("创建数据目录失败: {e}"));
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
/// 清空 performance_metrics + icon_cache 两表。均为可重建缓存，清空无风险。
/// clipboard_images 不在此清--它是用户复制的图片（用户数据），需通过独立的
/// clear_clipboard_images 按钮（有确认）清理。
#[tauri::command]
pub async fn clear_cache_db(app: tauri::AppHandle) -> Result<(), String> {
    let pools = app.state::<crate::infra::data::DbPools>();
    // 清空 performance_metrics
    crate::infra::data::perf::clear_all(&pools.cache)
        .await
        .map_err(|e| format!("清空 performance_metrics 失败: {e}"))?;
    // 清空 icon_cache
    crate::infra::data::icon_cache::clear_all(&pools.cache).await;
    // 0.16.0: VACUUM 收缩数据库文件
    let _ = crate::infra::data::compact(&pools.cache).await;
    tracing::info!("缓存库已清空（performance_metrics + icon_cache）");
    Ok(())
}

/// 设置页-存储：手动优化存储（0.17.0）。
///
/// 对四个数据库（config / history / ai / cache）无条件执行压缩
/// （VACUUM + wal_checkpoint(TRUNCATE)，WAL 模式下才能真正回收空间）。
/// 返回各库压缩前后 freelist 信息与是否成功，供前端展示。
#[tauri::command]
pub async fn optimize_storage(app: tauri::AppHandle) -> serde_json::Value {
    let pools = app.state::<crate::infra::data::DbPools>();

    let mut results = Vec::new();
    for (name, pool) in [
        ("config", &pools.config),
        ("history", &pools.history),
        ("ai", &pools.ai),
        ("cache", &pools.cache),
    ] {
        let before: (i64,) = sqlx::query_as("PRAGMA freelist_count")
            .fetch_one(pool)
            .await
            .unwrap_or((0,));
        // compact = VACUUM + wal_checkpoint(TRUNCATE)，WAL 模式下才能真正回收空间
        let (success, error) = match crate::infra::data::compact(pool).await {
            Ok(()) => (true, None),
            Err(e) => (false, Some(e)),
        };
        let after: (i64,) = sqlx::query_as("PRAGMA freelist_count")
            .fetch_one(pool)
            .await
            .unwrap_or((0,));
        tracing::info!(
            db = name,
            before = before.0,
            after = after.0,
            success,
            "optimize_storage: compact 完成"
        );
        results.push(serde_json::json!({
            "db": name,
            "freelist_before": before.0,
            "freelist_after": after.0,
            "success": success,
            "error": error,
        }));
    }

    serde_json::json!({ "results": results })
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

/// 读取网络策略的代理 URL（tauri updater 注入用，https 优先）。
///
/// 0.27.4：网络配置迁移到 `network` 分片/net 工厂后，代理统一从快照取。
fn updater_proxy_url() -> Option<String> {
    let policy = crate::infra::net::global().policy();
    policy.https.clone().or(policy.http)
}

/// 一次检查的结果（0.26.20 从 check_update command 提取，供设置页手动检查 /
/// 后台静默任务 / 主窗口内置动作三路共用）。Clone 供结果条目构造测试复用基准。
#[derive(Clone, Debug)]
pub(crate) struct UpdateCheckOutcome {
    pub has_update: bool,
    pub current_version: String,
    pub latest_version: String,
    pub release_url: String,
    pub release_notes: String,
    /// (version, notes, release_url)，从新到旧。
    pub updates: Vec<(String, String, String)>,
    /// (version, notes, release_url)，从新到旧、不做版本比较的最近已发布条目
    /// （含当前版本与更早版本），供「更新日志」入口展示。镜像降级/失败路径为空。
    pub recent_updates: Vec<(String, String, String)>,
    /// Some(错误描述) = 网络失败 / API 异常；此时 has_update 恒 false。
    pub error: Option<String>,
    /// 0.27.4 §3.6：主链（GitHub API）不可达、镜像 latest.json 判出版本。
    /// true = 版本可检查、多版本更新说明不可用。
    pub degraded: bool,
}

/// 检查 GitHub Release 是否有新版本（0.26.20 提取为共享函数；0.27.4 降级改造）。
///
/// 主链（§3.6）：GitHub API `/repos/{owner/repo}/releases?per_page=10`（时间倒序）
/// → 取第一条非 prerelease 为「最新发布版」→ `version_gt` 比较；全部大于当前
/// 版本的已发布条目作为 `updates` 数组（跳版升级可见中间各版内容）。
///
/// **镜像降级**：主链失败（网络/非 2xx/解析失败）后，经既有
/// `github_proxy_candidates` 拉镜像 `latest.json` **只判版本**，返回
/// `degraded = true`（版本可检查、说明不可用）；镜像也全失败才是「检查失败」，
/// 不能报「已是最新」。主链与全部镜像共用 [`UPDATE_CHECK_BUDGET`] 总时限。
///
/// **网络策略**：client 经 net 工厂 Update 类别——按所选 scheme 走代理，
/// 空配置显式直连（§3.5）；镜像同样遵守网络策略，不静默旁路直连。
pub(crate) async fn perform_update_check(_app: &tauri::AppHandle) -> UpdateCheckOutcome {
    let current = env!("CARGO_PKG_VERSION");
    let repo = env!("CARGO_PKG_REPOSITORY");
    // 从 "https://github.com/owner/repo" 提取 "owner/repo"
    let repo_path = repo
        .trim_start_matches("https://github.com/")
        .trim_start_matches("http://github.com/")
        .trim_end_matches('/');

    let api_url = format!("https://api.github.com/repos/{repo_path}/releases?per_page=10");
    let client = crate::infra::net::client(crate::infra::net::RequestCategory::Update);

    tokio::time::timeout(UPDATE_CHECK_BUDGET, async {
        match check_via_github_api(&client, &api_url, repo_path, current).await {
            Ok(outcome) => outcome,
            Err(main_error) => {
                tracing::warn!(%main_error, "check_update: GitHub API 主链失败，尝试镜像降级");
                let endpoint = format!(
                    "https://github.com/{repo_path}/releases/latest/download/latest.json"
                );
                let candidates = crate::infra::utils::mirrors::github_proxy_candidates(&endpoint);
                match check_via_mirror_latest(&client, &candidates, repo_path, current).await {
                    Ok(mut outcome) => {
                        outcome.degraded = true;
                        tracing::info!(
                            latest = %outcome.latest_version,
                            has_update = outcome.has_update,
                            "check_update: 镜像降级成功（版本可检查、说明不可用）"
                        );
                        outcome
                    }
                    Err(mirror_error) => {
                        tracing::warn!(%mirror_error, "check_update: 镜像 latest.json 亦不可达");
                        error_outcome(
                            current,
                            format!("检查失败：{main_error}；镜像亦不可达（{mirror_error}）"),
                        )
                    }
                }
            }
        }
    })
    .await
    .unwrap_or_else(|_| error_outcome(current, "更新检查超时".to_string()))
}

/// 主链：GitHub API releases 列表 → 完整 outcome（notes + 多版本 updates）。
///
/// 返回 `Err` 表示主链不可用（网络错误 / 非 2xx / 解析失败），由调用方决定
/// 是否走镜像降级。
async fn check_via_github_api(
    client: &reqwest::Client,
    api_url: &str,
    repo_path: &str,
    current: &str,
) -> Result<UpdateCheckOutcome, String> {
    let resp = client.get(api_url).send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("GitHub API 返回 {}", resp.status()));
    }
    let releases = resp
        .json::<Vec<serde_json::Value>>()
        .await
        .map_err(|e| format!("响应解析失败: {e}"))?;

    // 「最新发布版」= 列表第一条非 prerelease（GitHub /releases 按创建时间倒序）
    let latest_release = releases
        .iter()
        .find(|r| !r["prerelease"].as_bool().unwrap_or(false));
    let latest = latest_release
        .and_then(|r| r["tag_name"].as_str())
        .map(|t| t.trim_start_matches('v').to_string())
        .unwrap_or_default();
    let release_url = latest_release
        .and_then(|r| r["html_url"].as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("https://github.com/{repo_path}/releases/latest"));
    // 0.17.1 §3.8：最新版的 release notes；release.yml 模板固定含「下载方式 +
    // SmartScreen 提示」样板段，只保留更新日志正文
    let release_notes = latest_release
        .map(|r| extract_changelog(r["body"].as_str().unwrap_or("")))
        .unwrap_or_default();
    // 0.25.19：全部待更新版本（> 当前版本，从新到旧），供前端分组展示
    let pending = collect_pending_updates(&releases, current);
    // 0.28.9：最近已发布条目（含当前版本），供「更新日志」入口随时可看
    let recent = collect_recent_updates(&releases, RECENT_UPDATES_LIMIT);

    let has_update = crate::app::update_reminder::version_gt(&latest, current);
    if has_update {
        tracing::info!(current, latest, pending = pending.len(), "发现新版本");
    } else {
        tracing::debug!(current, latest, "已是最新版本");
    }

    Ok(UpdateCheckOutcome {
        has_update,
        current_version: current.to_string(),
        latest_version: latest,
        release_url,
        release_notes,
        updates: pending,
        recent_updates: recent,
        error: None,
        degraded: false,
    })
}

/// 镜像降级（§3.6）：镜像 latest.json **只判版本**。
///
/// latest.json（tauri updater 产物）只含最新一版的 `version`/`notes`，无
/// 多版本 releases 列表——降级语义固定为「版本可检查、说明不可用」，
/// 不用单版 notes 冒充完整更新日志。全部镜像失败返回 Err（调用方报检查失败）。
async fn check_via_mirror_latest(
    client: &reqwest::Client,
    candidates: &[String],
    repo_path: &str,
    current: &str,
) -> Result<UpdateCheckOutcome, String> {
    let mut last_error = String::new();
    for url in candidates {
        let resp = match client.get(url).send().await {
            Ok(r) => r,
            Err(e) => {
                last_error = e.to_string();
                continue;
            }
        };
        if !resp.status().is_success() {
            last_error = format!("HTTP {}", resp.status());
            continue;
        }
        let json: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                last_error = format!("latest.json 解析失败: {e}");
                continue;
            }
        };
        // 版本校验：非空字符串才可信（§3.6 校验版本与响应格式）
        let Some(version) = json["version"].as_str().map(str::trim).filter(|v| !v.is_empty())
        else {
            last_error = "latest.json 缺少 version 字段".to_string();
            continue;
        };
        let version = version.trim_start_matches('v').to_string();
        let has_update = crate::app::update_reminder::version_gt(&version, current);
        return Ok(UpdateCheckOutcome {
            has_update,
            current_version: current.to_string(),
            latest_version: version,
            release_url: format!("https://github.com/{repo_path}/releases/latest"),
            release_notes: String::new(),
            updates: Vec::new(),
            recent_updates: Vec::new(),
            error: None,
            degraded: false, // 由调用方统一置位
        });
    }
    Err(last_error)
}

/// 构造失败结果（`has_update` 恒 false，`latest_version` 留空）。
fn error_outcome(current: &str, error: String) -> UpdateCheckOutcome {
    UpdateCheckOutcome {
        has_update: false,
        current_version: current.to_string(),
        latest_version: String::new(),
        release_url: String::new(),
        release_notes: String::new(),
        updates: Vec::new(),
        recent_updates: Vec::new(),
        error: Some(error),
        degraded: false,
    }
}

/// 更新检查主链 + 镜像降级共用的总时限（§3.6：共享总时限与取消信号）。
const UPDATE_CHECK_BUDGET: std::time::Duration = std::time::Duration::from_secs(15);

/// 设置页-关于：检查 GitHub Release 是否有新版本（0.25.19 起一次拉取最近多条）。
///
/// 返回 JSON：
/// - 成功：`{ has_update, current_version, latest_version, release_url,
///   release_notes, updates: [{version, notes, release_url}],
///   recent_updates: [{version, notes, release_url}] }`
///   （updates 为待更新版本；recent_updates 为最近若干已发布版本——含当前
///   版本与更早版本，供「更新日志」入口展示；两者均从新到旧，可为空数组）
/// - 网络失败：`{ has_update: false, current_version, error: "..." }`
///
/// 0.26.20：网络与解析逻辑提取为 [`perform_update_check`]（与后台静默检查共用）；
/// 命令侧同时经 `update_reminder::record_check_outcome` 落库检查状态（手动
/// 检查也推进 24h 节流与 `latest_version`，后台任务不重复查）。
#[tauri::command]
pub async fn check_update(app: tauri::AppHandle) -> serde_json::Value {
    let outcome = perform_update_check(&app).await;
    let latest = outcome.error.is_none().then(|| outcome.latest_version.clone());
    crate::app::update_reminder::record_check_outcome(&app, latest.as_deref()).await;
    outcome_to_json(&outcome)
}

/// 记录「已读屏蔽」：把当前可用更新版本写入 `dismissed_version`（0.26.20）。
/// 建议槽的显式关闭与 Tab 采纳都调用本命令——用户已知情，本版本不再出现；
/// 发布更新的版本后自动恢复提示。无可用版本时 no-op。
#[tauri::command]
pub async fn dismiss_update_version(app: tauri::AppHandle) -> Result<(), String> {
    crate::app::update_reminder::dismiss_latest(&app).await;
    Ok(())
}

/// 检查结果 → 前端 JSON（设置页 check_update 命令专用；主窗口内置动作走
/// run_builtin_action 的 ReturnItems 协议，条目由 update_reminder::check_result_items 构造）。
fn outcome_to_json(outcome: &UpdateCheckOutcome) -> serde_json::Value {
    if let Some(error) = &outcome.error {
        return serde_json::json!({
            "has_update": false,
            "current_version": outcome.current_version,
            "error": error,
        });
    }
    serde_json::json!({
        "has_update": outcome.has_update,
        "current_version": outcome.current_version,
        "latest_version": outcome.latest_version,
        "release_url": outcome.release_url,
        "release_notes": outcome.release_notes,
        "degraded": outcome.degraded,
        "updates": outcome.updates
            .iter()
            .map(|(version, notes, url)| serde_json::json!({
                "version": version,
                "notes": notes,
                "release_url": url,
            }))
            .collect::<Vec<_>>(),
        "recent_updates": outcome.recent_updates
            .iter()
            .map(|(version, notes, url)| serde_json::json!({
                "version": version,
                "notes": notes,
                "release_url": url,
            }))
            .collect::<Vec<_>>(),
    })
}

/// 从 release 数组筛出待更新条目（0.25.19）：跳过 prerelease 与不大于当前
/// 版本的条目，每条经 `extract_changelog` 过滤模板，返回
/// (version, notes, release_url)，保持 GitHub 返回的时间倒序（新→旧）。
fn collect_pending_updates(
    releases: &[serde_json::Value],
    current: &str,
) -> Vec<(String, String, String)> {
    releases
        .iter()
        .filter(|r| !r["prerelease"].as_bool().unwrap_or(false))
        .filter_map(|r| {
            let version = r["tag_name"].as_str()?.trim_start_matches('v');
            if !crate::app::update_reminder::version_gt(version, current) {
                return None;
            }
            let notes = extract_changelog(r["body"].as_str().unwrap_or(""));
            let url = r["html_url"].as_str().unwrap_or("").to_string();
            Some((version.to_string(), notes, url))
        })
        .collect()
}

/// 「更新日志」入口展示的最近发布条目数（GitHub 单次拉 per_page=10，
/// 取其非 prerelease 子集的前 5 条足够看「最近更新了什么」）。
const RECENT_UPDATES_LIMIT: usize = 5;

/// 从 release 数组取最近已发布条目（0.28.9）：不做版本比较（当前版本与更早
/// 版本都在内），跳过 prerelease，每条经 `extract_changelog` 过滤模板，
/// 保持 GitHub 返回的时间倒序（新→旧），最多 `limit` 条。
fn collect_recent_updates(
    releases: &[serde_json::Value],
    limit: usize,
) -> Vec<(String, String, String)> {
    releases
        .iter()
        .filter(|r| !r["prerelease"].as_bool().unwrap_or(false))
        .filter_map(|r| {
            let version = r["tag_name"].as_str()?.trim_start_matches('v');
            let notes = extract_changelog(r["body"].as_str().unwrap_or(""));
            let url = r["html_url"].as_str().unwrap_or("").to_string();
            Some((version.to_string(), notes, url))
        })
        .take(limit)
        .collect()
}

/// 从 release body 截取实际更新内容：release.yml 固定模板的
/// 「下载方式 + SmartScreen 提示」样板段在「### 更新日志」标记之前，滤掉后
/// 只保留其后的正文；无标记（手工编辑/历史 release）时原样返回，宁多勿缺。
fn extract_changelog(notes: &str) -> String {
    match notes.split_once("### 更新日志") {
        Some((_, changelog)) => changelog.trim().to_string(),
        None => notes.trim().to_string(),
    }
}

// ── 0.25.10 应用内一键更新（tauri-plugin-updater）──────────────────────────

/// 一键更新任务互斥标志——防止重复点击造成并发下载/安装。
/// 成功路径上进程会被安装器 exit(0) 接管，标志无需复位也无害（进程即终态）；
/// 所有可返回的错误路径与防御分支都显式复位。
static UPDATE_INSTALL_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

/// 进度事件发送 helper（stage: downloading / installing / failed）。
fn emit_update_progress(
    app: &tauri::AppHandle,
    stage: &str,
    downloaded: u64,
    total: Option<u64>,
    error: Option<&str>,
) {
    if let Err(e) = app.emit(
        crate::infra::event_names::EventNames::UPDATE_INSTALL_PROGRESS,
        serde_json::json!({
            "stage": stage,
            "downloaded": downloaded,
            "total": total,
            "error": error,
        }),
    ) {
        tracing::debug!(%e, "emit UPDATE_INSTALL_PROGRESS 失败");
    }
}

/// 构建 updater（注入全局代理与 endpoint 候选链）并检查是否有可用更新。
///
/// endpoint 候选链：主站 latest.json → 加速代理（签名只校验文件字节，与源无关）。
/// 本函数内部可用 `?` 自由早退——互斥标志的复位统一由调用方负责。
async fn check_updater_update(
    app: &tauri::AppHandle,
) -> Result<Option<tauri_plugin_updater::Update>, String> {
    use tauri_plugin_updater::UpdaterExt;

    let proxy_url = updater_proxy_url();
    let endpoint = format!(
        "{}/releases/latest/download/latest.json",
        env!("CARGO_PKG_REPOSITORY").trim_end_matches('/')
    );
    let endpoints = crate::infra::utils::mirrors::github_proxy_candidates(&endpoint)
        .iter()
        .map(|u| tauri::Url::parse(u).map_err(|e| format!("更新地址无效 {u}: {e}")))
        .collect::<Result<Vec<_>, String>>()?;

    let mut builder = app.updater_builder();
    builder = builder
        .endpoints(endpoints)
        .map_err(|e| format!("更新地址配置失败: {e}"))?;
    if let Some(ref proxy) = proxy_url {
        match tauri::Url::parse(proxy) {
            Ok(u) => builder = builder.proxy(u),
            Err(e) => tracing::warn!(proxy = %proxy, %e, "install_update: 代理配置无效，回退直连"),
        }
    }
    builder = builder.on_before_exit(|| {
        tracing::info!("install_update: 应用即将退出，由安装器接管（passive 安装 + 自动重启）");
    });

    let updater = builder
        .build()
        .map_err(|e| format!("更新器初始化失败: {e}"))?;
    updater
        .check()
        .await
        .map_err(|e| format!("检查更新失败: {e}"))
}

/// 设置页-关于：一键下载并安装更新。
///
/// 流程：拉取 latest.json（主站 → 加速代理候选链，见
/// `infra::utils::mirrors::github_proxy_candidates`）→ minisign 验签（公钥嵌在
/// tauri.conf.json）→ 版本比较（仅升不降）→ 下载（进度经 `UPDATE_INSTALL_PROGRESS`
/// 事件推送，≥200ms 节流）→ 启动 NSIS 安装器（passive 进度条）→ 应用退出，
/// 安装完成后自动重启。
///
/// 检查在本 command 内同步执行：返回 `{status: "no_update"}` 或
/// `{status: "started", version}`；下载/安装在后台任务进行，结果只通过事件
/// 表达——检查阶段失败走 invoke 错误通道，下载阶段失败走 failed 事件
/// （此时 invoke 已返回，只能靠事件）。
#[tauri::command]
pub async fn install_update(app: tauri::AppHandle) -> Result<serde_json::Value, String> {
    // debug 构建（cargo tauri dev / xtask release --debug）没有安装器注册信息，
    // 安装器无法完成覆盖安装，直接给出可行动的错误而不是让插件报 cryptic 错误
    if cfg!(debug_assertions) {
        return Err("开发模式不支持应用内更新（未走安装器），请使用 release 安装包".into());
    }

    if UPDATE_INSTALL_IN_PROGRESS
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err("更新任务进行中，请等待完成或重启应用".into());
    }

    // 检查阶段：任何失败路径（含无更新）都必须复位互斥标志后再返回
    let update = match check_updater_update(&app).await {
        Ok(Some(update)) => update,
        Ok(None) => {
            UPDATE_INSTALL_IN_PROGRESS.store(false, Ordering::Release);
            tracing::debug!("install_update: 无可用更新");
            return Ok(serde_json::json!({ "status": "no_update" }));
        }
        Err(e) => {
            UPDATE_INSTALL_IN_PROGRESS.store(false, Ordering::Release);
            tracing::warn!(%e, "install_update: 检查更新失败");
            return Err(e);
        }
    };
    let target_version = update.version.clone();
    tracing::info!(version = %target_version, "install_update: 开始下载更新");

    let version_for_task = target_version.clone();
    let app_for_task = app.clone();
    tauri::async_runtime::spawn(async move {
        // 共享进度快照：下载闭包写入，下载完成闭包与错误路径读取。
        // Cell/Mutex 保证闭包仍满足 Send（跨 await 持有）。
        let progress = std::sync::Arc::new(std::sync::Mutex::new((0u64, None::<u64>)));
        let last_emit = std::cell::Cell::new(None::<std::time::Instant>);

        let prog_chunk = progress.clone();
        let app_chunk = app_for_task.clone();
        let prog_finish = progress.clone();
        let app_for_finish = app_for_task.clone();
        let result = update
            .download_and_install(
                move |chunk, total| {
                    let mut p = prog_chunk.lock().unwrap();
                    // 插件回调给 usize，内部统一 u64 计数
                    p.0 = p.0.saturating_add(chunk as u64);
                    p.1 = total;
                    drop(p);
                    // 节流：≥200ms 才发一条，避免 IPC 事件洪泛
                    let now = std::time::Instant::now();
                    if last_emit
                        .get()
                        .is_none_or(|t| now.duration_since(t) >= std::time::Duration::from_millis(200))
                    {
                        last_emit.set(Some(now));
                        let p = prog_chunk.lock().unwrap();
                        emit_update_progress(&app_chunk, "downloading", p.0, p.1, None);
                    }
                },
                move || {
                    // 下载完成 → 安装器即将启动、进程即将退出；发 installing 终态
                    let (downloaded, total) = *prog_finish.lock().unwrap();
                    emit_update_progress(&app_for_finish, "installing", downloaded, total, None);
                },
            )
            .await;

        match result {
            Ok(()) => {
                // Windows 上安装阶段进程会被 exit(0)，正常不应到达这里；
                // 防御性复位，避免标志位卡死
                UPDATE_INSTALL_IN_PROGRESS.store(false, Ordering::Release);
                tracing::info!(
                    version = %version_for_task,
                    "install_update: 下载安装流程返回（预期进程已被安装器接管）"
                );
            }
            Err(e) => {
                UPDATE_INSTALL_IN_PROGRESS.store(false, Ordering::Release);
                tracing::warn!(version = %version_for_task, error = %e, "install_update: 下载/安装失败");
                let (downloaded, total) = *progress.lock().unwrap();
                emit_update_progress(
                    &app_for_task,
                    "failed",
                    downloaded,
                    total,
                    Some(&e.to_string()),
                );
            }
        }
    });

    Ok(serde_json::json!({ "status": "started", "version": target_version }))
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

/// 焦点元素 UIA 诊断（0.25.21）：返回最近一次焦点捕获（热键唤起 show 前捕获的
/// 目标应用焦点元素）与当前系统焦点的 ControlType/ClassName/进程信息。
///
/// 用途：实测各编辑器（Typora/记事本/Obsidian/终端）暴露的控件类型真值，
/// 定稿 `is_text_input_control` 白名单与 caret 兜底策略。诊断流程：
/// 目标应用输入框聚焦 → Alt+C/Alt+Space 唤起 → devtools 调用本命令看 `captured`。
#[tauri::command]
pub async fn debug_uia_focus() -> Result<serde_json::Value, String> {
    // COM 属性读是跨进程阻塞调用（spec-backend：阻塞走 spawn_blocking）
    let current =
        tauri::async_runtime::spawn_blocking(crate::infra::platform::uia::current_focus_info)
            .await
            .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({
        "captured": crate::infra::platform::uia::last_capture_info(),
        "current": current,
    }))
}

/// 获取性能统计概览（设置页 → 调试 Tab）。
#[tauri::command]
pub async fn get_perf_overview(app: tauri::AppHandle) -> serde_json::Value {
    let pool = &app.state::<crate::infra::data::DbPools>().cache;
    crate::infra::utils::perf::get_overview(pool).await
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
    crate::infra::utils::perf::query_percentiles(pool, &category, &name, limit.unwrap_or(100)).await
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
    crate::infra::utils::perf::query_slow(pool, &category, threshold_ms, limit.unwrap_or(20)).await
}

/// 查询最近 N 条性能指标。
#[tauri::command]
pub async fn get_perf_recent(
    app: tauri::AppHandle,
    limit: Option<i64>,
) -> Vec<crate::infra::utils::perf::PerformanceMetric> {
    let pool = &app.state::<crate::infra::data::DbPools>().cache;
    crate::infra::utils::perf::query_recent(pool, limit.unwrap_or(100)).await
}

/// 导出性能报告（JSON 格式）。
/// 弹出保存文件对话框，用户选择路径后写入文件，返回保存的路径（取消时返回 null）。
#[tauri::command]
pub async fn export_perf_report(app: tauri::AppHandle) -> Result<Option<String>, String> {
    let pool = &app.state::<crate::infra::data::DbPools>().cache;
    let report = crate::infra::utils::perf::export_report(pool).await;

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
    let rows = crate::infra::data::perf::clear_all(pool).await?;
    let _ = crate::infra::data::compact(pool).await; // VACUUM + WAL checkpoint 回收空间
    Ok(rows)
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

/// WAL 模式下数据库的真实磁盘占用 = `.db` + `-wal` + `-shm` 三文件之和。
///
/// 只统计 `.db` 会漏掉未 checkpoint 的 WAL 数据，数字严重偏小（实测缓存库
/// `.db` 60KB 而 `-wal` 达 2.8MB）。存储页展示与清理统计都用此口径。
fn db_size_bytes(data_dir: &std::path::Path, file: &str) -> u64 {
    let mut total = file_size(&data_dir.join(file));
    total += file_size(&data_dir.join(format!("{file}-wal")));
    total += file_size(&data_dir.join(format!("{file}-shm")));
    total
}

/// 语义化版本比较：a > b 则返回 true。
///
/// 优先用 `semver` 库严格比较（支持 pre-release / build metadata），
/// 解析失败时 fallback 到简单数字比较（兼容非标准版本号如 `0.9`）。
/// 0.26.20：version_gt / version_gt_fallback 迁至 `app::update_reminder`
///（检查结果与「可用更新」判定共用同一比较）。

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

// ── 0.16.6 卸载清理能力 ──────────────────────────────────────────────────────

/// 设置页-存储：获取完整清理信息（0.16.6）。
///
/// 探测 Blink 在系统中占用的全部数据，供用户在"一键清理"前确认范围：
/// - 四库 SQLite 文件总大小
/// - 日志目录大小
/// - Python/STT 环境目录大小
/// - Skills 目录大小
/// - Credential Manager 中 `blink/*` 密钥条数
#[tauri::command]
pub async fn get_cleanup_info() -> serde_json::Value {
    let data_dir = crate::infra::utils::paths::app_data_dir();

    // 数据目录总大小
    let data_dir_size = if data_dir.exists() {
        dir_size_bytes(&data_dir)
    } else {
        0
    };

    // 四库文件大小
    let db_files = [
        "blink_config.db",
        "blink_history.db",
        "blink_ai.db",
        "blink_cache.db",
    ];
    let db_total: u64 = db_files.iter().map(|f| db_size_bytes(&data_dir, f)).sum();

    // 日志目录
    let logs_dir = crate::infra::utils::paths::logs_dir();
    let logs_size = if logs_dir.exists() {
        dir_size_bytes(&logs_dir)
    } else {
        0
    };

    // Python/STT 环境
    let python_dir = crate::infra::utils::paths::python_dir();
    let python_size = if python_dir.exists() {
        dir_size_bytes(&python_dir)
    } else {
        0
    };

    // Skills 目录
    let skills_dir = crate::infra::utils::paths::skills_global_dir();
    let skills_size = if skills_dir.exists() {
        dir_size_bytes(&skills_dir)
    } else {
        0
    };

    // Credential Manager 中 blink 密钥条数
    // 0.25.16-fix: 全量枚举 + Rust 端 contains 过滤——原 `blink*` 前缀过滤自 0.17.11
    // 起漏计 v1 命名（{pid}/key.blink）；而 `*blink*` 通配 CredEnumerateW 不支持
    // （ERROR_NOT_FOUND 被当空列表），故不依赖 CM 通配语法，三代命名一处覆盖
    #[cfg(target_os = "windows")]
    let secret_count = {
        match crate::infra::platform::secret::enumerate_blink_secrets_anywhere() {
            Ok(secrets) => secrets.len(),
            Err(e) => {
                tracing::warn!(error = %e, "get_cleanup_info: 枚举密钥失败");
                0
            }
        }
    };
    #[cfg(not(target_os = "windows"))]
    let secret_count = 0usize;

    serde_json::json!({
        "data_dir": data_dir.display().to_string(),
        "data_dir_size_mb": bytes_to_mb(data_dir_size),
        "db_total_mb": bytes_to_mb(db_total),
        "logs_size_mb": bytes_to_mb(logs_size),
        "python_size_mb": bytes_to_mb(python_size),
        "skills_size_mb": bytes_to_mb(skills_size),
        "secret_count": secret_count,
    })
}

/// 设置页-存储：一键清理全部 Blink 数据（0.16.6 + 0.17.11 顺序调整）。
///
/// 清理范围：
/// 1. **先读 AIConfig 拿到 provider id 列表**（0.17.11——删数据目录后配置不可读）
/// 2. 关闭 DB 连接池
/// 3. 删除 `%APPDATA%\blink` 整个数据目录（含四库、日志、Python 环境、Skills 等）
/// 4. 批量删除密钥（按步骤 1 读到的 id 列表逐个调 `delete_secret`）
///
/// **默认拒绝**——前端必须弹确认对话框（默认选否），用户明确点确认后才调用。
/// 返回每个清理目标的 `Result`，前端按项展示成功/失败。
#[tauri::command]
pub async fn cleanup_all_data(app: tauri::AppHandle) -> serde_json::Value {
    let mut results: Vec<(String, Result<(), String>)> = Vec::new();

    // 0.17.11: 先读 AIConfig 拿到要清理的 provider id 列表
    // （删数据目录后配置不可读，delete_all_blink_secrets 需要这个列表）
    use crate::infra::platform::secret::SecretRef;
    let secret_refs: Vec<SecretRef> = {
        let pools = app.state::<crate::infra::data::DbPools>();
        let ai_config =
            crate::app::config::ConfigStore::get::<crate::app::ai_config::AIConfig>(&pools.config)
                .await;
        let mut refs: Vec<SecretRef> = ai_config
            .providers
            .iter()
            .filter(|p| p.kind.requires_secret())
            .map(|p| SecretRef::ai(&p.id))
            .collect();
        // STT 云端密钥也需清理（SecretRef::stt_cloud，v2 命名 blink:stt:cloud:key）
        refs.push(SecretRef::stt_cloud());
        // 插件密钥字段也需清理（0.25.16——从已加载 manifest 的 secret 字段收集；
        // 删数据目录后插件配置不可读，但 manifest 在内存 PluginEngine 里仍可用）
        if let Some(engine) =
            app.try_state::<std::sync::Arc<crate::domain::plugin::PluginEngine>>()
        {
            for plugin in engine.all_plugins() {
                for f in plugin.manifest().settings_schema.iter().filter(|f| f.secret) {
                    refs.push(SecretRef::plugin(plugin.id(), &f.key));
                }
            }
        }
        refs
    };

    // 1. 关闭 DB 连接池（释放文件占用，否则 remove_dir_all 会失败）
    {
        let pools = app.state::<crate::infra::data::DbPools>();
        let db_names = ["config", "history", "ai", "cache"];
        let pools_vec = [
            pools.config.clone(),
            pools.history.clone(),
            pools.ai.clone(),
            pools.cache.clone(),
        ];
        for (name, pool) in db_names.iter().zip(pools_vec.iter()) {
            tracing::info!("cleanup_all_data: 关闭 {name} 库连接池");
            pool.close().await;
        }
    }

    // 2. 删除数据目录
    let data_dir = crate::infra::utils::paths::app_data_dir();
    if data_dir.exists() {
        tracing::info!(path = %data_dir.display(), "cleanup_all_data: 删除数据目录");
        match std::fs::remove_dir_all(&data_dir) {
            Ok(()) => {
                tracing::info!("cleanup_all_data: 数据目录已删除");
                results.push(("数据目录".to_string(), Ok(())));
            }
            Err(e) => {
                tracing::warn!(error = %e, "cleanup_all_data: 删除数据目录失败");
                results.push((
                    "数据目录".to_string(),
                    Err(format!("删除数据目录失败: {e}")),
                ));
            }
        }
    } else {
        results.push(("数据目录".to_string(), Ok(()))); // 目录不存在视为成功
    }

    // 3. 清理密钥（0.17.11: 按步骤 0 读到的引用列表逐个删；0.25.15 入参改 SecretRef）
    #[cfg(target_os = "windows")]
    {
        let delete_results =
            crate::infra::platform::secret::delete_all_blink_secrets(&secret_refs);
        let mut secret_ok = true;
        let mut secret_errors = Vec::new();
        for (target, result) in delete_results {
            match result {
                Ok(()) => {}
                Err(e) => {
                    secret_ok = false;
                    secret_errors.push(format!("{target}: {e}"));
                }
            }
        }
        if secret_ok {
            results.push(("密钥清理".to_string(), Ok(())));
        } else {
            results.push(("密钥清理".to_string(), Err(secret_errors.join("; "))));
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        results.push(("密钥清理".to_string(), Ok(())));
    }

    // 汇总
    let success_count = results.iter().filter(|(_, r)| r.is_ok()).count();
    let failed_count = results.len() - success_count;

    tracing::info!(success_count, failed_count, "cleanup_all_data: 清理完成");

    serde_json::json!({
        "results": results.iter().map(|(target, result)| {
            serde_json::json!({
                "target": target,
                "success": result.is_ok(),
                "error": result.as_ref().err(),
            })
        }).collect::<Vec<_>>(),
        "success_count": success_count,
        "failed_count": failed_count,
    })
}

/// 打开主题调试窗口（0.25.11）：设置页"调试"Tab 入口。
///
/// 纯静态单页验收工具（theme-debug.html），窗口 close=hide 外壳复用、不预热不常驻，
/// 不参与 Alt+Space 主链路。
#[tauri::command]
pub async fn show_theme_debug_cmd(app: tauri::AppHandle) -> Result<(), String> {
    crate::infra::platform::window::show_theme_debug_window(&app)
}

#[cfg(test)]
mod tests {
    use super::{collect_pending_updates, collect_recent_updates, extract_changelog};

    /// release.yml 生成的完整模板形态（照抄 `.github/workflows/release.yml` 的 body 块）
    const TEMPLATE_BODY: &str = "## Blink v0.25.17\n\n### 下载\n- **MSI 安装包**（`.msi`）：标准 Windows 安装程序\n- **NSIS 安装包**（`-setup.exe`）：轻量安装程序\n\n> 首次运行未签名，Windows SmartScreen 可能拦截，点击「更多信息 → 仍要运行」即可。\n\n### 更新日志\n\n新增一键自动更新功能，优化浅色主题表现。\n\n调整快捷键支持单功能键。";

    #[test]
    fn changelog_extracts_body_after_marker_and_drops_template() {
        let out = extract_changelog(TEMPLATE_BODY);
        assert!(out.starts_with("新增一键自动更新功能"));
        assert!(out.ends_with("调整快捷键支持单功能键。"));
        assert!(!out.contains("MSI 安装包"));
        assert!(!out.contains("SmartScreen"));
        assert!(!out.contains("### 更新日志"));
    }

    #[test]
    fn changelog_falls_back_to_full_body_without_marker() {
        let manual = "手工编辑的 release，没有模板段";
        assert_eq!(extract_changelog(manual), manual);
    }

    #[test]
    fn changelog_empty_body_stays_empty() {
        assert_eq!(extract_changelog(""), "");
        assert_eq!(extract_changelog("  \n "), "");
    }

    /// 构造 GitHub `/releases` 列表条目（含模板 body 的最小形态）
    fn release_item(tag: &str, prerelease: bool, changelog: &str) -> serde_json::Value {
        serde_json::json!({
            "tag_name": tag,
            "prerelease": prerelease,
            "html_url": format!("https://github.com/chenbihao/blink/releases/tag/{tag}"),
            "body": format!("## Blink {tag}\n\n### 下载\n- **MSI 安装包**\n\n### 更新日志\n\n{changelog}"),
        })
    }

    #[test]
    fn pending_updates_keeps_newer_stable_releases_in_api_order() {
        // GitHub /releases 按创建时间倒序——fixture 顺序即真实返回顺序
        let releases = vec![
            release_item("v0.25.20", false, "乙功能"),
            release_item("v0.25.21-beta", true, "预发布不进列表"),
            release_item("v0.25.19", false, "甲功能"),
            release_item("v0.25.18", false, "当前版本不进列表"),
        ];
        let out = collect_pending_updates(&releases, "0.25.18");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].0, "0.25.20");
        assert_eq!(out[1].0, "0.25.19");
        assert!(out[0].1.contains("乙功能"));
        assert!(!out[0].1.contains("MSI 安装包"));
        assert!(out[0].2.ends_with("/tag/v0.25.20"));
    }

    #[test]
    fn pending_updates_empty_when_nothing_newer() {
        let releases = vec![
            release_item("v0.25.18", false, "当前版本"),
            release_item("v0.25.19-beta", true, "预发布"),
        ];
        assert!(collect_pending_updates(&releases, "0.25.18").is_empty());
        assert!(collect_pending_updates(&[], "0.25.18").is_empty());
    }

    /// 0.28.9：recent 不做版本比较——当前版本与更早版本都在内，跳过 prerelease，
    /// 保持时间倒序，截断到 limit。
    #[test]
    fn recent_updates_includes_current_and_older_in_api_order() {
        let releases = vec![
            release_item("v0.25.20", false, "乙功能"),
            release_item("v0.25.21-beta", true, "预发布不进列表"),
            release_item("v0.25.19", false, "当前版本日志"),
            release_item("v0.25.18", false, "更早版本"),
        ];
        let out = collect_recent_updates(&releases, 5);
        let versions: Vec<&str> = out.iter().map(|(v, _, _)| v.as_str()).collect();
        assert_eq!(versions, ["0.25.20", "0.25.19", "0.25.18"]);
        assert!(out[1].1.contains("当前版本日志"));
        assert!(!out[0].1.contains("MSI 安装包"), "模板段应被过滤");
        assert!(out[2].2.ends_with("/tag/v0.25.18"));
    }

    /// 0.28.9：recent 截断到 limit（超出部分丢弃），空列表返回空。
    #[test]
    fn recent_updates_truncates_to_limit_and_handles_empty() {
        let releases: Vec<serde_json::Value> = (0..8)
            .map(|i| release_item(&format!("v0.25.{:02}", 30 - i), false, "x"))
            .collect();
        assert_eq!(collect_recent_updates(&releases, 5).len(), 5);
        assert_eq!(collect_recent_updates(&releases, 5)[0].0, "0.25.30");
        assert!(collect_recent_updates(&[], 5).is_empty());
    }

    // ── 0.27.4 主链/镜像降级（本地 TcpListener 桩，沿用 wire_tests 手法）──────

    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{SocketAddr, TcpListener};
    use std::sync::{Arc, Mutex};

    /// 单用途 HTTP 桩：按序服务 n 个请求，回写固定 body；记录请求 path。
    fn spawn_http_stub(responses: Vec<(u16, String)>) -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let paths: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = paths.clone();
        std::thread::spawn(move || {
            for (status, body) in responses {
                let Ok((stream, _)) = listener.accept() else { break };
                let mut reader = BufReader::new(stream);
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).is_err() {
                    break;
                }
                let path = request_line.split_whitespace().nth(1).unwrap_or("").to_string();
                sink.lock().unwrap().push(path);
                let mut content_length = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() {
                        break;
                    }
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':')
                        && name.trim().eq_ignore_ascii_case("content-length")
                    {
                        content_length = value.trim().parse().unwrap_or(0);
                    }
                }
                if content_length > 0 {
                    let mut body = vec![0u8; content_length];
                    let _ = reader.read_exact(&mut body);
                }
                let reason = match status {
                    200 => "OK",
                    404 => "Not Found",
                    _ => "Error",
                };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len(),
                );
                let _ = reader.get_mut().write_all(response.as_bytes());
                let _ = reader.get_mut().flush();
            }
        });
        (addr, paths)
    }

    fn stub_client() -> reqwest::Client {
        reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(5))
            .connect_timeout(std::time::Duration::from_secs(3))
            .build()
            .expect("client")
    }

    /// 主链成功：releases 列表解析出完整 outcome（notes + 多版本 pending）。
    #[tokio::test]
    async fn main_chain_github_api_parses_full_outcome() {
        let releases = serde_json::json!([
            release_item("v0.27.4", false, "主链更新日志"),
            release_item("v0.27.5-beta", true, "预发布"),
            release_item("v0.27.3", false, "上一版"),
        ])
        .to_string();
        let (addr, _) = spawn_http_stub(vec![(200, releases)]);
        let url = format!("http://{addr}/repos/o/r/releases?per_page=10");

        let out = super::check_via_github_api(&stub_client(), &url, "o/r", "0.27.3")
            .await
            .expect("主链应成功");
        assert!(out.has_update);
        assert_eq!(out.latest_version, "0.27.4");
        assert!(out.release_notes.contains("主链更新日志"));
        assert_eq!(out.updates.len(), 1, "只有 0.27.4 > 0.27.3");
        // recent 含当前版本（0.27.3）且不做版本比较，跳过 prerelease
        let recent_versions: Vec<&str> =
            out.recent_updates.iter().map(|(v, _, _)| v.as_str()).collect();
        assert_eq!(recent_versions, ["0.27.4", "0.27.3"]);
        assert!(!out.degraded);
        assert!(out.error.is_none());
    }

    /// 主链非 2xx / 坏 JSON → Err（由调用方决定降级）。
    #[tokio::test]
    async fn main_chain_non_2xx_and_bad_json_return_err() {
        let (addr, _) = spawn_http_stub(vec![(404, "{}".to_string())]);
        let url = format!("http://{addr}/releases");
        assert!(super::check_via_github_api(&stub_client(), &url, "o/r", "0.27.3").await.is_err());

        let (addr2, _) = spawn_http_stub(vec![(200, "not json".to_string())]);
        let url2 = format!("http://{addr2}/releases");
        assert!(super::check_via_github_api(&stub_client(), &url2, "o/r", "0.27.3").await.is_err());
    }

    /// 镜像降级：latest.json 只判版本，notes 空、updates 空、degraded 未置位
    /// （由 perform_update_check 统一置位）。
    #[tokio::test]
    async fn mirror_latest_json_reports_version_without_notes() {
        let latest = serde_json::json!({
            "version": "0.27.4",
            "notes": "镜像也有 notes，但降级语义固定不采用",
            "platforms": {},
        })
        .to_string();
        let (addr, _) = spawn_http_stub(vec![(200, latest)]);
        let candidates = vec![format!("http://{addr}/releases/latest/download/latest.json")];

        let out = super::check_via_mirror_latest(&stub_client(), &candidates, "o/r", "0.27.3")
            .await
            .expect("镜像应成功");
        assert!(out.has_update);
        assert_eq!(out.latest_version, "0.27.4");
        assert!(out.release_notes.is_empty(), "镜像降级不提供更新说明");
        assert!(out.updates.is_empty(), "latest.json 无多版本列表");
        assert!(!out.degraded, "degraded 由调用方置位");
        assert!(out.error.is_none());
    }

    /// 镜像版本不高于当前 → 降级 + 无更新（不是失败）。
    #[tokio::test]
    async fn mirror_latest_json_no_update_is_valid_outcome() {
        let latest = serde_json::json!({"version": "0.27.3", "platforms": {}}).to_string();
        let (addr, _) = spawn_http_stub(vec![(200, latest)]);
        let candidates = vec![format!("http://{addr}/latest.json")];

        let out = super::check_via_mirror_latest(&stub_client(), &candidates, "o/r", "0.27.3")
            .await
            .expect("版本可检查就是成功降级");
        assert!(!out.has_update);
        assert_eq!(out.latest_version, "0.27.3");
    }

    /// 镜像坏响应（缺 version）/ 全部不可达 → Err（检查失败，不报「已是最新」）。
    #[tokio::test]
    async fn mirror_all_fail_returns_err_not_latest() {
        // 缺 version 字段 + 桩关闭后连接失败：全部候选失败 → Err（不报「已是最新」）
        let (addr, _) = spawn_http_stub(vec![(200, r#"{"notes":"no version"}"#.to_string())]);
        let base = format!("http://{addr}");
        let candidates = (0..2).map(|i| format!("{base}/mirror{i}/latest.json")).collect::<Vec<_>>();
        let _ = super::check_via_mirror_latest(&stub_client(), &candidates, "o/r", "0.27.3")
            .await
            .expect_err("坏响应 + 连接失败必须整体失败，不得报「已是最新」");
    }
}
