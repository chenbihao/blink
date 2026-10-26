//! plugin 域命令（0.14.6 §2.4 从 commands.rs 拆分）。
//!
//! 0.28.1：新增本地包安装/卸载/安装记录查询命令——安装走
//! `PluginInstaller`（暂存/校验/提交）+ `PluginRuntimePublisher`（契约校验/
//! owner 注册/快照发布/路由联动）+ 撤销联动取消待确认。官方目录下载在
//! 0.28.5 接入；当前通道为 dev 本地包（zip 或解包目录）。

use super::ai::cs_count_skills;
use super::plugin_ui::reject_plugin_window;
use super::stt::copy_dir_recursive;
use tauri::Manager;

use crate::app::command_error::CommandError;

/// 读取当前插件子进程代理 env（network 分片同源快照；None = 直连）。
async fn plugin_env_proxy(
    pool: &sqlx::SqlitePool,
) -> Option<(String, String)> {
    let cfg = crate::app::config::init_network_config(pool).await;
    cfg.has_proxy().then(|| cfg.to_policy().plugin_env_tuple())
}

/// 从已提交的版本目录构造插件句柄（含 v2 握手目录注入）。
fn handle_from_record(
    installer: &std::sync::Arc<crate::domain::plugin::PluginInstaller>,
    record: &blink_infra::plugin_storage::InstallRecord,
    proxy: Option<(String, String)>,
) -> Result<std::sync::Arc<crate::domain::plugin::PluginHandle>, CommandError> {
    let dir = installer
        .active_version_dir(&record.plugin_id)
        .ok_or_else(|| {
            CommandError::new("invalid_state", "安装记录存在但版本目录缺失", false)
        })?;
    let manifest = crate::domain::plugin::PluginManifest::from_path(&dir.join("manifest.json"))
        .map_err(|e| {
            CommandError::new("invalid_manifest", format!("manifest 解析失败: {e}"), false)
        })?;
    let handle = std::sync::Arc::new(crate::domain::plugin::PluginHandle::new(
        std::sync::Arc::new(manifest),
        dir.clone(),
        proxy,
    ));
    let storage = installer.storage();
    handle.set_dirs(crate::domain::plugin::PluginDirs {
        data: storage.data_dir(&record.plugin_id).to_string_lossy().into_owned(),
        cache: storage.cache_dir(&record.plugin_id).to_string_lossy().into_owned(),
        datasets: storage.datasets_dir(&record.plugin_id).to_string_lossy().into_owned(),
    });
    Ok(handle)
}

/// 安装本地插件包（0.28.1 dev 通道：zip 归档或解包目录）。
///
/// 流程：installer 暂存/校验/提交（spawn_blocking，归档 IO）→ 构造新句柄 →
/// 发布器发布目标集合（同 id 替换 = 更新）→ 撤销的旧 tool 取消待确认。
/// 0.28.2：更新/安装后关闭该插件旧窗口（撤销旧页面会话——旧代次窗口不继承
/// 新实现）；插件窗口自身不得调用本命令（生态敏感命令门禁）。
#[tauri::command]
pub async fn install_plugin_package(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    path: String,
) -> Result<blink_infra::plugin_storage::InstallRecord, CommandError> {
    reject_plugin_window(&window)?;
    let installer = app
        .state::<std::sync::Arc<crate::domain::plugin::PluginInstaller>>()
        .inner()
        .clone();
    let src = std::path::PathBuf::from(&path);
    if !src.exists() {
        return Err(CommandError::new(
            "not_found",
            format!("包路径不存在: {path}"),
            false,
        ));
    }

    // 归档 IO 隔离到 blocking 线程（spec-backend §一）
    let installer_for_io = installer.clone();
    let src_for_io = src.clone();
    let record = tauri::async_runtime::spawn_blocking(move || {
        if src_for_io.is_dir() {
            installer_for_io.install_dir(&src_for_io)
        } else {
            installer_for_io.install_zip(&src_for_io)
        }
    })
    .await
    .map_err(|e| CommandError::new("internal", format!("安装任务失败: {e}"), true))?
    .map_err(|e| {
        CommandError::new("install_failed", format!("插件安装失败: {e}"), false)
    })?;

    // 发布目标集合：同 id 替换（更新语义）
    let engine = app
        .state::<std::sync::Arc<crate::domain::plugin::PluginEngine>>()
        .inner()
        .clone();
    let publisher = app
        .state::<std::sync::Arc<crate::domain::plugin::PluginRuntimePublisher>>()
        .inner()
        .clone();
    let pool = app.state::<crate::infra::data::DbPools>().config.clone();
    let proxy = plugin_env_proxy(&pool).await;
    let new_handle = handle_from_record(&installer, &record, proxy)?;
    let mut target = engine.all_plugins();
    target.retain(|h| h.id() != record.plugin_id);
    target.push(new_handle);
    let report = publisher.publish(target).await.map_err(|e| {
        CommandError::new(
            "publish_failed",
            format!("安装已提交但发布失败（重启后生效）: {e}"),
            true,
        )
    })?;
    // 旧代次 tool 的待确认/会话信任一并失效
    if let Some(pc) = app.try_state::<std::sync::Arc<crate::domain::ai::tool_adapter::PendingConfirms>>() {
        pc.cancel_for_tools(&report.revoked_tool_ids).await;
    }
    // 0.28.2：撤销旧页面会话（更新语义下旧代次窗口关闭，重开走新包）
    crate::app::plugin_surface::close_plugin_windows(&app, &record.plugin_id);
    tracing::info!(
        plugin = %record.plugin_id,
        version = %record.version,
        generation = report.generation,
        "插件包安装并发布完成"
    );
    Ok(record)
}

/// 卸载插件（0.28.1）。`keep_data=true` 保留用户数据目录。
#[tauri::command]
pub async fn uninstall_plugin(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    plugin_id: String,
    keep_data: bool,
) -> Result<serde_json::Value, CommandError> {
    reject_plugin_window(&window)?;
    // 0.28.2：先关窗口（撤销页面会话），再走运行时撤销
    crate::app::plugin_surface::close_plugin_windows(&app, &plugin_id);
    let installer = app
        .state::<std::sync::Arc<crate::domain::plugin::PluginInstaller>>()
        .inner()
        .clone();
    let installer_io = installer.clone();
    let pid = plugin_id.clone();
    let outcome = tauri::async_runtime::spawn_blocking(move || {
        installer_io.uninstall(&pid, keep_data)
    })
    .await
    .map_err(|e| CommandError::new("internal", format!("卸载任务失败: {e}"), true))?
    .map_err(|e| CommandError::new("uninstall_failed", format!("插件卸载失败: {e}"), false))?;

    // 从运行时撤销（能力/路由/快照）
    let engine = app
        .state::<std::sync::Arc<crate::domain::plugin::PluginEngine>>()
        .inner()
        .clone();
    let publisher = app
        .state::<std::sync::Arc<crate::domain::plugin::PluginRuntimePublisher>>()
        .inner()
        .clone();
    let mut target = engine.all_plugins();
    target.retain(|h| h.id() != plugin_id);
    let report = publisher.publish(target).await.map_err(|e| {
        CommandError::new(
            "publish_failed",
            format!("卸载已记录但运行时撤销失败（重启后生效）: {e}"),
            true,
        )
    })?;
    if let Some(pc) = app.try_state::<std::sync::Arc<crate::domain::ai::tool_adapter::PendingConfirms>>() {
        pc.cancel_for_tools(&report.revoked_tool_ids).await;
    }

    Ok(match &outcome {
        crate::domain::plugin::UninstallOutcome::Removed => serde_json::json!({
            "removed": true,
            "pending_files": 0,
        }),
        crate::domain::plugin::UninstallOutcome::RemovedPendingFiles { pending_paths } => {
            serde_json::json!({
                "removed": true,
                "pending_files": pending_paths.len(),
                "note": "部分文件被占用，将在下次启动时清理",
            })
        }
    })
}

/// 查询用户目录安装记录（0.28.1；设置页"已安装"数据源之一）。
#[tauri::command]
pub async fn list_plugin_installations(
    app: tauri::AppHandle,
) -> Result<Vec<blink_infra::plugin_storage::InstallRecord>, CommandError> {
    let installer = app
        .state::<std::sync::Arc<crate::domain::plugin::PluginInstaller>>()
        .inner()
        .clone();
    installer.list_installed().map_err(|e| {
        CommandError::new("io", format!("读取安装记录失败: {e}"), true)
    })
}

/// 安装本地数据包（0.28.4 词库通道：zip 归档或解包目录，dev 本地包；
/// 官方目录下载在 0.28.5 接入）。归档 IO/逐文件哈希校验走 blocking 线程。
///
/// 安装提交后**不重启插件进程**——插件按 active.json 指针在下次查询自行
/// 切换版本；readiness 由设置页下一轮 `get_plugin_surface_status` 重算。
#[tauri::command]
pub async fn install_plugin_dataset(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    path: String,
) -> Result<blink_infra::plugin_storage::DatasetRecord, CommandError> {
    reject_plugin_window(&window)?;
    let installer = app
        .state::<std::sync::Arc<crate::domain::plugin::DatasetInstaller>>()
        .inner()
        .clone();
    let src = std::path::PathBuf::from(&path);
    if !src.exists() {
        return Err(CommandError::new(
            "not_found",
            format!("数据包路径不存在: {path}"),
            false,
        ));
    }
    let installer_io = installer.clone();
    let record = tauri::async_runtime::spawn_blocking(move || {
        if src.is_dir() {
            installer_io.install_dir(&src)
        } else {
            installer_io.install_zip(&src)
        }
    })
    .await
    .map_err(|e| CommandError::new("internal", format!("数据包安装任务失败: {e}"), true))?
    .map_err(|e| {
        CommandError::new("install_failed", format!("数据包安装失败: {e}"), false)
    })?;
    tracing::info!(
        plugin = %record.plugin_id,
        dataset = %record.dataset_id,
        version = %record.version,
        "数据包安装完成"
    );
    Ok(record)
}

/// 卸载数据包（0.28.4）。插件进程可能仍持有 DB 句柄——文件删不净时如实
/// 返回待清理数（下次启动 recover 重试），不假报已删除。
#[tauri::command]
pub async fn uninstall_plugin_dataset(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    plugin_id: String,
    dataset_id: String,
) -> Result<serde_json::Value, CommandError> {
    reject_plugin_window(&window)?;
    let installer = app
        .state::<std::sync::Arc<crate::domain::plugin::DatasetInstaller>>()
        .inner()
        .clone();
    let installer_io = installer.clone();
    let outcome = tauri::async_runtime::spawn_blocking(move || {
        installer_io.uninstall(&plugin_id, &dataset_id)
    })
    .await
    .map_err(|e| CommandError::new("internal", format!("数据包卸载任务失败: {e}"), true))?
    .map_err(|e| {
        CommandError::new("uninstall_failed", format!("数据包卸载失败: {e}"), false)
    })?;
    Ok(match outcome {
        crate::domain::plugin::UninstallDatasetOutcome::Removed => {
            serde_json::json!({"removed": true, "pending_files": 0})
        }
        crate::domain::plugin::UninstallDatasetOutcome::RemovedPendingFiles {
            pending_paths,
        } => serde_json::json!({
            "removed": true,
            "pending_files": pending_paths.len(),
            "note": "部分文件被占用，将在下次启动时清理",
        }),
    })
}

/// 获取所有已加载插件的信息（设置页用）。已含 enabled + settings（0.5.1）。
#[tauri::command]
pub async fn get_plugins(app: tauri::AppHandle) -> Vec<serde_json::Value> {
    let engine = app.state::<std::sync::Arc<crate::domain::plugin::PluginEngine>>();
    // 读当前语言,供 manifest 配置文案按 locale 取值(设置页中英双语)
    let pool = &app.state::<crate::infra::data::DbPools>().config;
    let lang = crate::app::config::get_config(pool).await.language;
    engine.list_plugins(&lang)
}

/// 读插件密钥字段的首尾掩码（设置页 placeholder，0.25.16）。
///
/// 不返回明文——只返回 `format_hint` 掩码（如 `sk-a••••cdef`）；未配置返回 None。
/// 字段必须在 manifest 声明 `secret:true`（防任意字段探测）。
#[tauri::command]
pub async fn get_plugin_secret_hint(
    app: tauri::AppHandle,
    plugin_id: String,
    field_key: String,
) -> Result<Option<String>, String> {
    let engine = app.state::<std::sync::Arc<crate::domain::plugin::PluginEngine>>();
    Ok(engine.secret_hint(&plugin_id, &field_key))
}

/// 清除插件密钥字段（设置页密钥框「清除」按钮，0.25.16）。幂等——未配置视为已删。
#[tauri::command]
pub async fn clear_plugin_secret(
    app: tauri::AppHandle,
    plugin_id: String,
    field_key: String,
    expected_revision: u64,
) -> Result<(), String> {
    let engine = app.state::<std::sync::Arc<crate::domain::plugin::PluginEngine>>();
    engine
        .clear_secret(&plugin_id, &field_key, expected_revision)
        .await?;
    use tauri::Emitter;
    let _ = app.emit(
        crate::domain::event_names::EventNames::CONFIG_CHANGED,
        serde_json::json!({"key":"plugin_config"}),
    );
    tracing::info!(plugin = %plugin_id, field = %field_key, "插件密钥已清除");
    Ok(())
}

/// 设置页的受限地点搜索桥接；不接受任意 tool ID，不绕过 Capability 门禁。
/// `resolver` 只选择内建解析来源（auto/open_meteo/qweather），不接受临时 Host/Key。
#[tauri::command]
pub async fn search_weather_locations(
    app: tauri::AppHandle,
    query: String,
    country_code: Option<String>,
    admin: Option<String>,
    limit: Option<u64>,
    resolver: Option<String>,
) -> Result<serde_json::Value, crate::app::command_error::CommandError> {
    use crate::domain::capability::{
        CapabilityRegistry, CapabilityResult, InvocationOrigin, InvokeContext, RuntimeCapabilities,
    };
    let env = app
        .state::<std::sync::Arc<crate::app::domain_env::TauriDomainEnv>>()
        .inner()
        .clone();
    let registry = app.state::<std::sync::Arc<CapabilityRegistry>>();
    let ctx = InvokeContext {
        env: env.as_ref(),
        origin: InvocationOrigin::LocalSurface,
        runtime: RuntimeCapabilities {
            surface: Some(env.as_ref()),
            main_process: true,
            desktop_session: true,
        },
        deadline: Some(std::time::Instant::now() + std::time::Duration::from_secs(10)),
    };
    let mut args = serde_json::json!({"query":query,"limit":limit.unwrap_or(10)});
    if let Some(country) = country_code.filter(|s| !s.trim().is_empty()) {
        args["country_code"] = serde_json::json!(country);
    }
    if let Some(admin) = admin.filter(|s| !s.trim().is_empty()) {
        args["admin"] = serde_json::json!(admin);
    }
    if let Some(resolver) = resolver.filter(|s| !s.trim().is_empty()) {
        if !matches!(resolver.as_str(), "auto" | "open_meteo" | "qweather") {
            return Err(crate::app::command_error::CommandError::new(
                "invalid_arguments",
                "解析来源必须是 auto、open_meteo 或 qweather",
                true,
            ));
        }
        args["resolver"] = serde_json::json!(resolver);
    }
    let result = registry
        .invoke("builtin_weather_search_locations", args, &ctx)
        .await
        .map_err(crate::app::command_error::CommandError::from)?;
    match result {
        CapabilityResult::Items { mut items } if items.len() == 1 => Ok(items.remove(0).data),
        _ => Err(crate::app::command_error::CommandError::new(
            "invalid_response",
            "地点搜索返回格式无效",
            true,
        )),
    }
}

/// 列出所有已发现的 Skill 条目（设置页展示用）。
///
/// 0.13.6: 返回带 `disabled` 标记的 SkillEntryWithStatus，前端用此渲染复选框。
#[tauri::command]
pub async fn list_skills(
    app: tauri::AppHandle,
) -> Vec<crate::domain::ai::skill::SkillEntryWithStatus> {
    use tauri::Manager;
    let chat = app.try_state::<std::sync::Arc<crate::domain::ai::chat_service::ChatService>>();
    match chat {
        Some(cs) => cs.skill_registry().all_with_status(),
        None => Vec::new(),
    }
}

/// 手动刷新 Skill 注册表——重新扫描所有启用的来源目录。
///
/// 从当前 AIConfig 读取 `skill_config.enabled_sources()`，调用 `ChatService::refresh_skills`。
/// 设置页「刷新」按钮调用。
#[tauri::command]
pub async fn refresh_skills(app: tauri::AppHandle) -> Result<usize, String> {
    use tauri::Manager;
    let pools = app.state::<crate::infra::data::DbPools>();
    let ai_config =
        crate::app::config::ConfigStore::get::<crate::app::ai_config::AIConfig>(&pools.config)
            .await;

    let chat = app
        .try_state::<std::sync::Arc<crate::domain::ai::chat_service::ChatService>>()
        .ok_or("ChatService 未初始化")?;
    chat.apply_skill_config(&ai_config.chat_config.skill_config);
    let count = cs_count_skills(&chat);
    tracing::info!(count, "Skill 注册表已手动刷新");
    Ok(count)
}

/// 打开指定来源的 Skill 目录（Explorer）。
///
/// 目录不存在时自动创建（方便用户放入 SKILL.md）。
/// `source` 参数："blink" / "claude" / "zcode"。
#[tauri::command]
pub async fn open_skill_dir(source: String) -> Result<(), String> {
    let skill_source = match source.as_str() {
        "blink" => crate::domain::ai::skill::SkillSource::Blink,
        "claude" => crate::domain::ai::skill::SkillSource::Claude,
        "zcode" => crate::domain::ai::skill::SkillSource::Zcode,
        _ => return Err(format!("未知的 Skill 来源: {source}")),
    };

    let dir = skill_source
        .directory()
        .ok_or_else(|| "无法解析 Skill 目录路径".to_string())?;

    // 目录不存在时先创建，避免 explorer 打开默认位置
    if !dir.exists() {
        std::fs::create_dir_all(&dir).map_err(|e| format!("创建 Skill 目录失败: {e}"))?;
    }

    std::process::Command::new("explorer.exe")
        .arg(&dir)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// 0.13.7: 枚举外部 Skill 来源（Claude / Codex / OpenCode / ZCode）。
///
/// 供设置页「导入 Skill」面板：下拉选应用 → 展示该应用目录下可导入的 skill 列表。
/// 返回每个来源的目录路径、是否存在、及其下已发现的 skill 概要（name/dir/description）。
#[tauri::command]
pub async fn list_external_skill_sources() -> Vec<crate::domain::ai::skill::ExternalSkillSourceInfo>
{
    crate::domain::ai::skill::list_external_sources()
}

/// 0.13.6: 导入 Skill 到 Blink 目录。
///
/// `source_path` = 源 SKILL.md 所在目录
/// `mode` = "symlink" | "copy"
/// 导入后在 `%APPDATA%\blink\skills\<name>\` 创建软链接或副本。
/// 软链接失败时自动降级为 Copy + 提示。
#[tauri::command]
pub async fn import_skill(source_path: String, mode: String) -> Result<String, String> {
    use crate::domain::ai::skill::SkillSource;

    let source_dir = std::path::Path::new(&source_path);
    if !source_dir.is_dir() {
        return Err(format!("源目录不存在: {source_path}"));
    }

    // 从目录名提取 Skill 名称
    let skill_name = source_dir
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or("无法从路径提取 Skill 名称")?;

    // 目标目录：%APPDATA%\blink\skills\<name>\
    let target_dir = SkillSource::Blink
        .directory()
        .ok_or("无法解析 Blink Skill 目录路径")?
        .join(skill_name);

    // 如果目标已存在，返回错误
    if target_dir.exists() {
        return Err(format!(
            "目标目录已存在: {}（请先删除或重命名）",
            target_dir.display()
        ));
    }

    // 确保父目录存在
    if let Some(parent) = target_dir.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建父目录失败: {e}"))?;
    }

    match mode.as_str() {
        "symlink" => {
            // 尝试创建符号链接
            match std::os::windows::fs::symlink_dir(source_dir, &target_dir) {
                Ok(()) => {
                    tracing::info!(
                        skill = %skill_name,
                        source = %source_dir.display(),
                        target = %target_dir.display(),
                        "Skill 已通过软链接导入"
                    );
                    Ok(format!(
                        "Skill '{skill_name}' 已通过软链接导入到 Blink 目录。\n双向同步：源目录变更自动反映。"
                    ))
                }
                Err(e) => {
                    // 降级为 Copy
                    tracing::warn!(
                        error = %e,
                        "软链接创建失败，降级为 Copy"
                    );
                    copy_dir_recursive(source_dir, &target_dir)?;
                    Ok(format!(
                        "软链接创建失败（{e}）。已降级为复制。\n如需软链接，请在 Windows 设置 → 隐私和安全性 → 开发者选项 中开启开发者模式。\nSkill '{skill_name}' 已通过复制导入到 Blink 目录。"
                    ))
                }
            }
        }
        _ => {
            copy_dir_recursive(source_dir, &target_dir)?;
            tracing::info!(
                skill = %skill_name,
                source = %source_dir.display(),
                target = %target_dir.display(),
                "Skill 已通过复制导入"
            );
            Ok(format!(
                "Skill '{skill_name}' 已通过复制导入到 Blink 目录。"
            ))
        }
    }
}

/// 0.13.6: 设置单个 Skill 的启用/禁用状态。
///
/// `skill_id` 格式：`name@source`（如 `"rust-debug@claude"`）。
/// 更新 AIConfig.skill_config.disabled_skills 并同步到运行时 SkillRegistry。
#[tauri::command]
pub async fn set_skill_enabled(
    app: tauri::AppHandle,
    skill_id: String,
    enabled: bool,
) -> Result<(), String> {
    use tauri::Manager;
    let pools = app.state::<crate::infra::data::DbPools>();
    let mut ai_config =
        crate::app::config::ConfigStore::get::<crate::app::ai_config::AIConfig>(&pools.config)
            .await;

    // 更新 disabled_skills
    let mut disabled = ai_config.chat_config.skill_config.disabled_skills.clone();
    if enabled {
        disabled.retain(|id| id != &skill_id);
    } else if !disabled.contains(&skill_id) {
        disabled.push(skill_id);
    }
    ai_config.chat_config.skill_config.disabled_skills = disabled;

    // 持久化
    crate::app::config::ConfigStore::set(&pools.config, &ai_config)
        .await
        .map_err(|e| e.to_string())?;

    // 同步到运行时
    if let Some(chat) =
        app.try_state::<std::sync::Arc<crate::domain::ai::chat_service::ChatService>>()
    {
        chat.update_skill_disabled(ai_config.chat_config.skill_config.disabled_skills.clone());
    }

    Ok(())
}

/// 保存编辑后的 SKILL.md 内容到指定 skill 目录。
#[tauri::command]
pub async fn save_skill_md(skill_dir: String, content: String) -> Result<(), String> {
    let dir = std::path::Path::new(&skill_dir);
    if !dir.exists() {
        std::fs::create_dir_all(dir).map_err(|e| format!("创建 Skill 目录失败: {e}"))?;
    }
    let skill_md_path = dir.join("SKILL.md");
    std::fs::write(&skill_md_path, &content).map_err(|e| format!("写入 SKILL.md 失败: {e}"))?;
    tracing::info!(path = %skill_md_path.display(), "SKILL.md 已保存");
    Ok(())
}

/// 读取指定 skill 目录中的 SKILL.md 内容（编辑用）。
#[tauri::command]
pub async fn get_skill_content(skill_dir: String) -> Result<String, String> {
    let skill_md_path = std::path::Path::new(&skill_dir).join("SKILL.md");
    if !skill_md_path.exists() {
        return Err("SKILL.md 不存在".to_string());
    }
    std::fs::read_to_string(&skill_md_path).map_err(|e| format!("读取 SKILL.md 失败: {e}"))
}

/// 删除指定 skill 目录（包含 SKILL.md 及同目录资源）。
#[tauri::command]
pub async fn delete_skill(skill_dir: String) -> Result<(), String> {
    let dir = std::path::Path::new(&skill_dir);
    if !dir.exists() {
        return Ok(()); // 已删除，幂等
    }
    std::fs::remove_dir_all(dir).map_err(|e| format!("删除 Skill 目录失败: {e}"))?;
    tracing::info!(dir = %dir.display(), "Skill 目录已删除");
    Ok(())
}
